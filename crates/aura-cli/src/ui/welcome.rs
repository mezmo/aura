use crossterm::style::{Color, Stylize};
use crossterm::{cursor, execute, terminal};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use super::state::BULLET_PALETTE;

/// Sentinel markers that wrap interpolated env-var values in the content
/// string so `print()` can render them in a lighter shade.
const ENV_START: char = '\x01';
const ENV_END: char = '\x02';

/// Number of frames in the fade-in animation.
const FADE_FRAMES: usize = 60;
/// Milliseconds between animation frames.
const FADE_INTERVAL_MS: u64 = 25;
/// Maximum random delay before a color instance begins fading (fraction of
/// total animation duration).  A value of 0.55 means some characters won't
/// start appearing until over halfway through.
const MAX_STAGGER: f32 = 0.55;
/// Range of easing exponents.  Each color instance picks a random exponent
/// in `MIN_EASE..=MAX_EASE` which controls how quickly it ramps to full
/// brightness.  Lower = faster initial pop, higher = slower gradual reveal.
const MIN_EASE: f32 = 1.2;
const MAX_EASE: f32 = 4.0;

/// Built-in welcome files compiled into the binary.
const BUILTIN_WELCOME_FILES: &[(&str, &str)] = &[
    (".welcome", include_str!("../../.welcome")),
    (
        ".welcome.no-color-codes",
        include_str!("../../.welcome.no-color-codes"),
    ),
];

/// Cached welcome state so the same content + colors are reused across
/// `/expand` toggles and `/resume` replays within a session.
#[derive(Clone)]
pub struct WelcomeState {
    content: String,
    text_color: Color,
    art_color: Color,
}

impl WelcomeState {
    /// Load the `.welcome` file and choose colors.
    /// When the art contains ANSI RGB codes, the text color is derived as a
    /// bright, high-contrast complement of the art's dominant hue.  Otherwise
    /// a random palette color is used.
    pub fn pick() -> Option<Self> {
        let content = pick_welcome_file()?;
        let (text_color, art_color) = if content.contains('\x1b') {
            let complement = complementary_text_color(&content);
            let (_, fallback) = random_color_pair();
            (complement, fallback)
        } else {
            random_color_pair()
        };
        Some(Self {
            content,
            text_color,
            art_color,
        })
    }

    /// Render the welcome banner instantly (no animation).
    /// Used for replays after `/expand`, `/stream`, `/clear`, etc.
    pub fn print_static(&self) {
        let mut stdout = io::stdout();

        // Leading blank line
        println!();

        for (line_idx, line) in self.content.lines().enumerate() {
            if line.contains('\x1b') {
                // ANSI art: pass through at full brightness (global_t = 1.0)
                println!("{}", scale_ansi_rgb_staggered(line, line_idx, 1.0));
            } else {
                let (art_part, text_part) = split_art_text(line);
                for c in art_part.chars() {
                    print!("{}", c.to_string().with(self.art_color));
                }
                let trimmed = text_part.trim_start();
                if !trimmed.is_empty() {
                    let leading = &text_part[..text_part.len() - trimmed.len()];
                    print!("{}", leading);
                    print_text_with_env_vars_static(trimmed, self.text_color);
                } else if !text_part.is_empty() {
                    print!("{}", text_part);
                }
                println!();
            }
        }

        // Trailing blank line
        println!();
        let _ = stdout.flush();
    }

    /// Render the welcome banner to stdout with a staggered fade-in animation.
    /// Each color instance gets a random delay before it begins fading from
    /// black to its target value, creating a scattered "materialise" effect
    /// over ~1 s with an ease-out quadratic curve.
    pub fn print(&self) {
        let line_count = self.content.lines().count();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, cursor::Hide);

        for frame in 0..FADE_FRAMES {
            let global_t = (frame + 1) as f32 / FADE_FRAMES as f32;

            // On subsequent frames, move cursor back up to overwrite.
            if frame > 0 {
                // +2 accounts for the blank lines above and below the banner
                let _ = execute!(stdout, cursor::MoveUp((line_count + 2) as u16));
            }

            // Leading blank line
            let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
            println!();

            for (line_idx, line) in self.content.lines().enumerate() {
                let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
                if line.contains('\x1b') {
                    println!("{}", scale_ansi_rgb_staggered(line, line_idx, global_t));
                } else {
                    let (art_part, text_part) = split_art_text(line);
                    for (ci, c) in art_part.chars().enumerate() {
                        let b = staggered_brightness(line_idx, ci, global_t);
                        print!("{}", c.to_string().with(scale_color(self.art_color, b)));
                    }
                    let text_offset = art_part.chars().count();
                    let trimmed = text_part.trim_start();
                    if !trimmed.is_empty() {
                        let leading = &text_part[..text_part.len() - trimmed.len()];
                        print!("{}", leading);
                        print_text_with_env_vars_staggered(
                            trimmed,
                            self.text_color,
                            line_idx,
                            text_offset,
                            global_t,
                        );
                    } else if !text_part.is_empty() {
                        print!("{}", text_part);
                    }
                    println!();
                }
            }

            // Trailing blank line
            let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
            println!();

            let _ = stdout.flush();

            if frame < FADE_FRAMES - 1 {
                thread::sleep(Duration::from_millis(FADE_INTERVAL_MS));
            }
        }
    }
}

/// Pick two distinct random colors from the palette using process ID + nanos
/// for entropy, so each app launch gets a genuinely different pair.
fn random_color_pair() -> (Color, Color) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let pid = std::process::id();
    let seed = nanos
        .wrapping_mul(2654435761)
        .wrapping_add(pid.wrapping_mul(31));
    let len = BULLET_PALETTE.len();
    let idx1 = (seed as usize) % len;
    let idx2 = (idx1 + 1 + ((seed >> 13) as usize) % (len - 1)) % len;
    let (r1, g1, b1) = BULLET_PALETTE[idx1];
    let (r2, g2, b2) = BULLET_PALETTE[idx2];
    (
        Color::Rgb {
            r: r1,
            g: g1,
            b: b1,
        },
        Color::Rgb {
            r: r2,
            g: g2,
            b: b2,
        },
    )
}

/// Extract all RGB values from ANSI `\x1b[38;2;R;G;Bm` sequences in the
/// content, compute the average hue, then return a bright, high-contrast
/// color at the complementary hue.  The result is light enough to read on
/// dark terminal backgrounds while visually harmonising with the art.
fn complementary_text_color(content: &str) -> Color {
    let mut total_r: u64 = 0;
    let mut total_g: u64 = 0;
    let mut total_b: u64 = 0;
    let mut count: u64 = 0;

    let mut rest = content;
    while let Some(esc_pos) = rest.find("\x1b[38;2;") {
        let after = &rest[esc_pos + 7..]; // skip past "\x1b[38;2;"
        if let Some(m_pos) = after.find('m') {
            let params = &after[..m_pos];
            let parts: Vec<&str> = params.split(';').collect();
            if parts.len() == 3
                && let (Ok(r), Ok(g), Ok(b)) = (
                    parts[0].parse::<u8>(),
                    parts[1].parse::<u8>(),
                    parts[2].parse::<u8>(),
                )
            {
                total_r += r as u64;
                total_g += g as u64;
                total_b += b as u64;
                count += 1;
            }
            rest = &after[m_pos + 1..];
        } else {
            break;
        }
    }

    if count == 0 {
        // Fallback: bright white if no colors found
        return Color::Rgb {
            r: 230,
            g: 230,
            b: 230,
        };
    }

    let avg_r = (total_r / count) as f32 / 255.0;
    let avg_g = (total_g / count) as f32 / 255.0;
    let avg_b = (total_b / count) as f32 / 255.0;

    // Convert average RGB to HSL
    let (hue, _sat, _light) = rgb_to_hsl(avg_r, avg_g, avg_b);

    // Complementary hue (180° opposite)
    let comp_hue = (hue + 180.0) % 360.0;

    // High brightness (0.85) + moderate saturation (0.65) for readability
    let (r, g, b) = hsl_to_rgb(comp_hue, 0.65, 0.82);
    Color::Rgb {
        r: (r * 255.0) as u8,
        g: (g * 255.0) as u8,
        b: (b * 255.0) as u8,
    }
}

/// Convert RGB (each 0.0–1.0) to HSL.  Returns (hue 0–360, sat 0–1, light 0–1).
fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let light = (max + min) / 2.0;

    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, light);
    }

    let delta = max - min;
    let sat = if light > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };

    let hue = if (max - r).abs() < 1e-6 {
        ((g - b) / delta) % 6.0
    } else if (max - g).abs() < 1e-6 {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };

    let hue = hue * 60.0;
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };

    (hue, sat, light)
}

/// Convert HSL back to RGB (each 0.0–1.0).
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s.abs() < 1e-6 {
        return (l, l, l);
    }

    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let h_norm = h / 360.0;

    let r = hue_to_rgb(p, q, h_norm + 1.0 / 3.0);
    let g = hue_to_rgb(p, q, h_norm);
    let b = hue_to_rgb(p, q, h_norm - 1.0 / 3.0);
    (r, g, b)
}

fn hue_to_rgb(p: f32, q: f32, mut t: f32) -> f32 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        return p + (q - p) * 6.0 * t;
    }
    if t < 0.5 {
        return q;
    }
    if t < 2.0 / 3.0 {
        return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
    }
    p
}

/// Characters considered "text" — letters, digits, common punctuation and MD markers.
fn is_text_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || c.is_ascii_punctuation()
        || c == ' '
        || c == '\t'
        || c == '\n'
        || c == '\r'
        || c == ENV_START
        || c == ENV_END
}

/// Split a line into (art_prefix, text_suffix) at the last non-text character.
fn split_art_text(line: &str) -> (&str, &str) {
    let mut last_art_byte = 0;
    let mut byte_pos = 0;
    for c in line.chars() {
        byte_pos += c.len_utf8();
        if !is_text_char(c) {
            last_art_byte = byte_pos;
        }
    }
    if last_art_byte == 0 {
        ("", line)
    } else {
        (&line[..last_art_byte], &line[last_art_byte..])
    }
}

/// Desaturate a colour by blending each channel toward a neutral gray.
/// `factor` 0.0 = unchanged, 1.0 = fully gray.  The result is a subtler,
/// more "pastel" shade that reads as lighter on dark backgrounds.
fn mute_color(color: Color, factor: f32) -> Color {
    const GRAY: f32 = 190.0;
    match color {
        Color::Rgb { r, g, b } => Color::Rgb {
            r: (r as f32 + (GRAY - r as f32) * factor) as u8,
            g: (g as f32 + (GRAY - g as f32) * factor) as u8,
            b: (b as f32 + (GRAY - b as f32) * factor) as u8,
        },
        other => other,
    }
}

/// Scale a colour's brightness by multiplying each RGB channel by `factor` (0.0–1.0).
fn scale_color(color: Color, factor: f32) -> Color {
    match color {
        Color::Rgb { r, g, b } => Color::Rgb {
            r: (r as f32 * factor) as u8,
            g: (g as f32 * factor) as u8,
            b: (b as f32 * factor) as u8,
        },
        other => other,
    }
}

/// Deterministic pseudo-random parameters for the color instance at
/// (`line`, `idx`).  Returns `(delay, exponent)` where `delay` is in
/// `0.0..=MAX_STAGGER` and `exponent` is in `MIN_EASE..=MAX_EASE`.
fn stagger_params(line: usize, idx: usize) -> (f32, f32) {
    let h1 = (line as u32)
        .wrapping_mul(2654435761)
        .wrapping_add((idx as u32).wrapping_mul(2246822519));
    let delay = (h1 % 10000) as f32 / 10000.0 * MAX_STAGGER;

    // Second independent hash for the easing exponent.
    let h2 = h1.wrapping_mul(1664525).wrapping_add(1013904223);
    let exponent = MIN_EASE + (h2 % 10000) as f32 / 10000.0 * (MAX_EASE - MIN_EASE);

    (delay, exponent)
}

/// Per-instance brightness given the global animation progress `global_t`
/// and the stagger / easing parameters derived from (`line`, `idx`).
/// Returns 0.0 while the instance hasn't started, then ease-out ramps to 1.0
/// at a rate determined by the instance's random exponent.
fn staggered_brightness(line: usize, idx: usize, global_t: f32) -> f32 {
    let (delay, exponent) = stagger_params(line, idx);
    if global_t <= delay {
        return 0.0;
    }
    let local_t = ((global_t - delay) / (1.0 - delay)).min(1.0);
    1.0 - (1.0 - local_t).powf(exponent) // ease-out with per-instance curve
}

/// Rewrite every `\x1b[38;2;R;G;Bm` sequence in `line`, giving each colour
/// instance its own staggered brightness derived from (`line_idx`, instance
/// index).  All other characters and escape sequences pass through unchanged.
fn scale_ansi_rgb_staggered(line: &str, line_idx: usize, global_t: f32) -> String {
    let mut result = String::with_capacity(line.len());
    let mut rest = line;
    let mut color_idx: usize = 0;

    while let Some(esc_pos) = rest.find('\x1b') {
        // Copy everything before the escape
        result.push_str(&rest[..esc_pos]);
        rest = &rest[esc_pos..];

        // Check for CSI: ESC [
        if rest.len() >= 2 && rest.as_bytes()[1] == b'[' {
            // Find the terminating 'm'
            if let Some(m_offset) = rest[2..].find('m') {
                let seq_end = 2 + m_offset + 1; // inclusive of 'm'
                let params = &rest[2..seq_end - 1];
                let parts: Vec<&str> = params.split(';').collect();
                if parts.len() == 5
                    && parts[0] == "38"
                    && parts[1] == "2"
                    && let (Ok(r), Ok(g), Ok(b)) = (
                        parts[2].parse::<u8>(),
                        parts[3].parse::<u8>(),
                        parts[4].parse::<u8>(),
                    )
                {
                    let brightness = staggered_brightness(line_idx, color_idx, global_t);
                    color_idx += 1;
                    let sr = (r as f32 * brightness) as u8;
                    let sg = (g as f32 * brightness) as u8;
                    let sb = (b as f32 * brightness) as u8;
                    result.push_str(&format!("\x1b[38;2;{};{};{}m", sr, sg, sb));
                    rest = &rest[seq_end..];
                    continue;
                }
                // Not a 24-bit foreground — emit the original sequence verbatim
                result.push_str(&rest[..seq_end]);
                rest = &rest[seq_end..];
            } else {
                // No terminating 'm' — emit ESC and advance one char
                result.push('\x1b');
                rest = &rest[1..];
            }
        } else {
            // Not a CSI sequence — emit ESC and advance
            result.push('\x1b');
            rest = &rest[1..];
        }
    }
    // Copy any remaining text after the last escape
    result.push_str(rest);
    result
}

/// Like `print_text_with_env_vars` but each character gets its own staggered
/// brightness so text materialises at different rates.
fn print_text_with_env_vars_staggered(
    text: &str,
    base_text_color: Color,
    line_idx: usize,
    char_offset: usize,
    global_t: f32,
) {
    let base_env_color = mute_color(base_text_color, 0.7);
    let mut ci = char_offset;
    let mut remaining = text;
    while let Some(start) = remaining.find(ENV_START) {
        let before = &remaining[..start];
        for ch in before.chars() {
            let b = staggered_brightness(line_idx, ci, global_t);
            print!("{}", ch.to_string().with(scale_color(base_text_color, b)));
            ci += 1;
        }
        remaining = &remaining[start + ENV_START.len_utf8()..];
        if let Some(end) = remaining.find(ENV_END) {
            let env_val = &remaining[..end];
            for ch in env_val.chars() {
                let b = staggered_brightness(line_idx, ci, global_t);
                print!("{}", ch.to_string().with(scale_color(base_env_color, b)));
                ci += 1;
            }
            remaining = &remaining[end + ENV_END.len_utf8()..];
        } else {
            break;
        }
    }
    for ch in remaining.chars() {
        let b = staggered_brightness(line_idx, ci, global_t);
        print!("{}", ch.to_string().with(scale_color(base_text_color, b)));
        ci += 1;
    }
}

/// Print text with env-var highlighting at full brightness (no animation).
fn print_text_with_env_vars_static(text: &str, base_text_color: Color) {
    let base_env_color = mute_color(base_text_color, 0.7);
    let mut remaining = text;
    while let Some(start) = remaining.find(ENV_START) {
        let before = &remaining[..start];
        for ch in before.chars() {
            print!("{}", ch.to_string().with(base_text_color));
        }
        remaining = &remaining[start + ENV_START.len_utf8()..];
        if let Some(end) = remaining.find(ENV_END) {
            let env_val = &remaining[..end];
            for ch in env_val.chars() {
                print!("{}", ch.to_string().with(base_env_color));
            }
            remaining = &remaining[end + ENV_END.len_utf8()..];
        } else {
            break;
        }
    }
    for ch in remaining.chars() {
        print!("{}", ch.to_string().with(base_text_color));
    }
}

/// Return the `~/.aura/` directory path.
fn aura_home_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".aura")
}

/// Ensure `~/.aura/` exists and seed it with the built-in welcome files
/// (compiled into the binary) if they aren't already present.
fn seed_home_welcome(aura_dir: &Path) {
    let _ = fs::create_dir_all(aura_dir);

    for (name, content) in BUILTIN_WELCOME_FILES {
        let dest = aura_dir.join(name);
        if !dest.exists() {
            let _ = fs::write(&dest, content);
        }
    }
}

/// Whether `key` looks like a valid environment variable name
/// (non-empty, ASCII alphanumeric / underscore only).
fn is_valid_env_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Replace `{{KEY}}` tokens with the corresponding environment variable value.
/// Only valid env-var names (alphanumeric + underscore) are interpolated;
/// other `{{...}}` patterns are kept literally.
/// Unset vars become empty strings. Unclosed `{{` is kept literally.
fn interpolate_env_vars(content: String) -> String {
    let mut result = String::with_capacity(content.len());
    let mut rest = content.as_str();

    while let Some(open) = rest.find("{{") {
        result.push_str(&rest[..open]);
        let after_open = &rest[open + 2..];
        if let Some(close) = after_open.find("}}") {
            let key = &after_open[..close];
            if is_valid_env_key(key) {
                let value = std::env::var(key).unwrap_or_default();
                result.push(ENV_START);
                result.push_str(&value);
                result.push(ENV_END);
            } else {
                // Not a valid env-var name — keep the literal {{...}}
                result.push_str("{{");
                result.push_str(key);
                result.push_str("}}");
            }
            rest = &after_open[close + 2..];
        } else {
            // Unclosed `{{` — keep literally and stop scanning
            result.push_str(&rest[open..]);
            rest = "";
            break;
        }
    }
    result.push_str(rest);
    result
}

/// Load the `.welcome` file from the current directory (preferred) or
/// `~/.aura/` (fallback), and interpolate env vars.
fn pick_welcome_file() -> Option<String> {
    let aura_dir = aura_home_dir();
    seed_home_welcome(&aura_dir);

    let cwd_welcome = Path::new(".welcome");
    let home_welcome = aura_dir.join(".welcome");

    let content = if cwd_welcome.exists() {
        fs::read_to_string(cwd_welcome).ok()
    } else {
        fs::read_to_string(&home_welcome).ok()
    };

    let content = content.filter(|c| !c.trim().is_empty())?;
    Some(interpolate_env_vars(content))
}
