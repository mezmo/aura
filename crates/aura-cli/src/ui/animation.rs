// ---------------------------------------------------------------------------
// Wave animations (WaveAnimation, ToolStatusAnimation, wave rendering)
// ---------------------------------------------------------------------------

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::{Attribute, Color, Stylize};
use crossterm::terminal;

use super::input_frame::{
    collapse_two_lines_above_frame, erase_input_frame, redraw_input_frame,
    update_bullet_above_tool_line, update_line_above_frame, update_two_lines_above_frame,
};
use super::mid_stream::{drain_stdin, render_input_line};
use super::state::STREAM_PANEL_DIRTY;
use super::state::{
    ACTIVE_ORCH_TOOLS, AGENT_REASONING, AGENT_REASONING_SEQ, CURSOR_ROW, FRAME_LINES,
    ORCH_LAST_TOOL_LINES, ORCH_SCROLLBACK_COUNTER, QUEUED_INPUT, QUEUED_WAVE_DIR, QUEUED_WAVE_POS,
    cache_anim_lines, check_resize, lock_term, random_bullet_color, term_size,
};
use super::stream_panel::render_stream_panel_in_place;

use super::orchestrator::format_orch_running;

/// Pulse a colour's brightness using a sine wave driven by `tick`.
pub(crate) fn pulse_color(base: Color, tick: u32) -> Color {
    let phase = ((tick as f32 * 0.15).sin() + 1.0) / 2.0;
    let brightness = 0.4 + phase * 0.6;
    if let Color::Rgb { r, g, b } = base {
        Color::Rgb {
            r: (r as f32 * brightness) as u8,
            g: (g as f32 * brightness) as u8,
            b: (b as f32 * brightness) as u8,
        }
    } else {
        base
    }
}

/// Render "● {label}" with a brightness wave across the label characters.
fn render_label_wave(label: &str, wave_pos: f32, bullet_color: Color, tick: u32) -> String {
    let mut result = String::new();
    result.push_str(&format!(
        "{} ",
        "●"
            .with(pulse_color(bullet_color, tick))
            .attribute(Attribute::Bold)
    ));
    for (i, ch) in label.chars().enumerate() {
        let distance = (i as f32 - wave_pos).abs();
        let brightness = (255.0 - distance * 50.0).clamp(100.0, 255.0) as u8;
        result.push_str(&format!(
            "{}",
            ch.to_string().with(Color::Rgb {
                r: brightness,
                g: brightness,
                b: brightness,
            })
        ));
    }
    result
}

/// Render "  ⎿ {text}" with a wide brightness wave.
fn render_thought_wave(text: &str, wave_pos: f32) -> String {
    let mut result = String::new();
    result.push_str(&format!("{}", "  ⎿ ".with(Color::DarkGrey)));
    for (i, ch) in text.chars().enumerate() {
        let distance = (i as f32 - wave_pos).abs();
        let brightness = (255.0 - distance * 2.5).clamp(100.0, 255.0) as u8;
        result.push_str(&format!(
            "{}",
            ch.to_string().with(Color::Rgb {
                r: brightness,
                g: brightness,
                b: brightness,
            })
        ));
    }
    result
}

/// Render queued input text with a brightness wave.
pub(crate) fn render_queued_wave(text: &str, wave_pos: f32) -> String {
    let mut result = String::new();
    result.push_str(&format!(
        "{} ",
        "❯".with(Color::Green).attribute(Attribute::Bold)
    ));
    let (width, _) = term_size();
    let max_len = (width as usize).saturating_sub(2);
    let display = if text.len() > max_len {
        &text[..max_len]
    } else {
        text
    };
    for (i, ch) in display.chars().enumerate() {
        let distance = (i as f32 - wave_pos).abs();
        let brightness = (255.0 - distance * 50.0).clamp(100.0, 255.0) as u8;
        result.push_str(&format!(
            "{}",
            ch.to_string().with(Color::Rgb {
                r: brightness,
                g: brightness,
                b: brightness,
            })
        ));
    }
    result
}

/// Advance the queued-input wave animation and re-render status row 3.
pub fn tick_queued_wave() {
    let queued = QUEUED_INPUT.lock().map(|g| g.clone()).unwrap_or_default();
    if queued.is_empty() {
        return;
    }
    let text_len = queued.chars().count() as f32;
    let max = (text_len - 1.0).max(0.0);

    if let (Ok(mut pos), Ok(mut dir)) = (QUEUED_WAVE_POS.lock(), QUEUED_WAVE_DIR.lock()) {
        *pos += *dir;
        if *pos >= max {
            *pos = max;
            *dir = -0.5;
        } else if *pos <= 0.0 {
            *pos = 0.0;
            *dir = 0.5;
        }
        let wave_pos = *pos;
        drop(pos);
        drop(dir);

        let mut stdout = io::stdout();
        let n = FRAME_LINES.load(Ordering::Relaxed) as i32;
        let r = CURSOR_ROW.load(Ordering::Relaxed) as i32;
        let _ = execute!(stdout, cursor::SavePosition);
        let down3 = n - r + 3;
        if down3 > 0 {
            let _ = execute!(
                stdout,
                cursor::MoveDown(down3 as u16),
                cursor::MoveToColumn(0)
            );
        }
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
        print!("{}", render_queued_wave(&queued, wave_pos));
        let _ = execute!(stdout, cursor::RestorePosition);
        let _ = stdout.flush();
    }
}

/// Render "  ⎿ ..." with a brightness wave across the 3 dots.
fn render_ellipsis_wave(dot_pos: usize) -> String {
    let mut result = String::new();
    result.push_str(&format!("{}", "  ⎿ ".with(Color::DarkGrey)));
    for i in 0..3usize {
        let brightness: u8 = if i == dot_pos { 160 } else { 80 };
        result.push_str(&format!(
            "{}",
            ".".with(Color::Rgb {
                r: brightness,
                g: brightness,
                b: brightness,
            })
        ));
    }
    result
}

/// Format a duration as a human-readable string.
pub(crate) fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else {
        format!("{:.1}s", secs)
    }
}

// ---------------------------------------------------------------------------
// WaveAnimation (Thinking animation)
// ---------------------------------------------------------------------------

pub struct WaveAnimation {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WaveAnimation {
    pub fn start(
        label: &str,
        thoughts: Vec<String>,
        input_buf: Arc<Mutex<String>>,
        cancel: Option<Arc<AtomicBool>>,
    ) -> (Self, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let stop_for_caller = stop.clone();

        let bullet_color = random_bullet_color();

        let init_top = render_label_wave(label, 0.0, bullet_color, 0);
        let init_bottom = if thoughts.is_empty() {
            render_ellipsis_wave(0)
        } else {
            render_thought_wave(&thoughts[0], 0.0)
        };
        println!("{}", init_top);
        println!("{}", init_bottom);
        cache_anim_lines(&init_top, &init_bottom);
        redraw_input_frame();

        let label = label.to_string();
        let label_len = label.chars().count();
        let handle = thread::spawn(move || {
            let mut ticks: u32 = 0;
            let mut wave_pos: f32 = 0.0;
            let wave_max = (label_len as f32 - 1.0).max(0.0);
            let mut wave_direction: f32 = 0.5;
            let mut dot_pos: usize = 0;
            let mut thought_idx: usize = 0;
            let mut thought_wave_pos: f32 = 0.0;
            let has_thoughts = !thoughts.is_empty();
            let mut last_reasoning_seq: u32 = AGENT_REASONING_SEQ.load(Ordering::Relaxed);
            let mut reasoning_wave_pos: f32 = 0.0;
            let mut reasoning_wave_dir: f32 = 2.0;

            while !stop_clone.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(20));
                if stop_clone.load(Ordering::Relaxed) {
                    return;
                }

                if check_resize().is_some() {
                    let _term = lock_term();
                    let mut stdout = io::stdout();
                    let _ = execute!(
                        stdout,
                        terminal::Clear(terminal::ClearType::All),
                        cursor::MoveTo(0, 0)
                    );
                    let top = render_label_wave(&label, wave_pos, bullet_color, ticks);
                    let bottom = {
                        let reasoning_text = AGENT_REASONING
                            .lock()
                            .map(|g| g.clone())
                            .unwrap_or_default();
                        if !reasoning_text.is_empty() {
                            let (tw, _) = term_size();
                            let max_len = (tw as usize).saturating_sub(4);
                            let display = if reasoning_text.chars().count() > max_len {
                                reasoning_text.chars().take(max_len).collect::<String>()
                            } else {
                                reasoning_text
                            };
                            render_thought_wave(&display, reasoning_wave_pos)
                        } else if has_thoughts {
                            let thought = &thoughts[thought_idx % thoughts.len()];
                            render_thought_wave(thought, thought_wave_pos)
                        } else {
                            render_ellipsis_wave(dot_pos)
                        }
                    };
                    println!("{top}");
                    println!("{bottom}");
                    cache_anim_lines(&top, &bottom);
                    redraw_input_frame();
                    if let Ok(buf) = input_buf.lock() {
                        render_input_line(&buf);
                    }
                    continue;
                }

                if let Ok(mut buf) = input_buf.lock() {
                    let prev_len = buf.len();
                    if drain_stdin(&mut buf)
                        && let Some(ref c) = cancel
                    {
                        c.store(true, Ordering::Relaxed);
                    }
                    if buf.len() != prev_len {
                        let _term = lock_term();
                        render_input_line(&buf);
                    }
                }

                ticks += 1;

                if ticks.is_multiple_of(3) {
                    wave_pos += wave_direction;
                    if wave_pos >= wave_max {
                        wave_pos = wave_max;
                        wave_direction = -0.5;
                    } else if wave_pos <= 0.0 {
                        wave_pos = 0.0;
                        wave_direction = 0.5;
                    }

                    let cur_reasoning_seq = AGENT_REASONING_SEQ.load(Ordering::Relaxed);
                    if cur_reasoning_seq != last_reasoning_seq {
                        last_reasoning_seq = cur_reasoning_seq;
                        reasoning_wave_pos = 0.0;
                        reasoning_wave_dir = 2.0;
                    }

                    let top = render_label_wave(&label, wave_pos, bullet_color, ticks);
                    let bottom = {
                        let reasoning_text = AGENT_REASONING
                            .lock()
                            .map(|g| g.clone())
                            .unwrap_or_default();
                        if !reasoning_text.is_empty() {
                            let (tw, _) = term_size();
                            let max_len = (tw as usize).saturating_sub(4);
                            let display = if reasoning_text.chars().count() > max_len {
                                reasoning_text.chars().take(max_len).collect::<String>()
                            } else {
                                reasoning_text
                            };
                            let display_max = (display.chars().count() as f32 - 1.0).max(0.0);
                            reasoning_wave_pos += reasoning_wave_dir;
                            if reasoning_wave_pos >= display_max {
                                reasoning_wave_pos = display_max;
                                reasoning_wave_dir = -2.0;
                            } else if reasoning_wave_pos <= 0.0 {
                                reasoning_wave_pos = 0.0;
                                reasoning_wave_dir = 2.0;
                            }
                            render_thought_wave(&display, reasoning_wave_pos)
                        } else if has_thoughts {
                            let thought = &thoughts[thought_idx % thoughts.len()];
                            let thought_max = (thought.chars().count() as f32 - 1.0).max(0.0);
                            thought_wave_pos += 0.5;
                            if thought_wave_pos > thought_max {
                                thought_wave_pos = 0.0;
                            }
                            render_thought_wave(thought, thought_wave_pos)
                        } else {
                            render_ellipsis_wave(dot_pos)
                        }
                    };
                    {
                        let _term = lock_term();
                        update_two_lines_above_frame(&top, &bottom);

                        // Update in-flight orchestrator tool bullets and durations
                        if let Ok(tools) = ACTIVE_ORCH_TOOLS.lock()
                            && !tools.is_empty()
                        {
                            let total_sb = ORCH_SCROLLBACK_COUNTER.load(Ordering::Relaxed);
                            let (_, th) = term_size();
                            let mut stdout = io::stdout();
                            let last_tool_map = ORCH_LAST_TOOL_LINES.lock().ok();
                            for tool in tools.iter() {
                                let bullet_up = (total_sb + 3).saturating_sub(tool.bullet_line_num);
                                let duration_up =
                                    (total_sb + 3).saturating_sub(tool.duration_line_num);
                                if bullet_up >= th as u32 || duration_up >= th as u32 {
                                    continue;
                                }
                                let is_last = last_tool_map
                                    .as_ref()
                                    .and_then(|m| m.get(&tool.task_id))
                                    .map(|info| info.bullet_line_num == tool.bullet_line_num)
                                    .unwrap_or(false);
                                let (b_prefix, d_prefix) = if is_last {
                                    (
                                        super::orchestrator::TREE_END_BULLET,
                                        super::orchestrator::TREE_END_DURATION,
                                    )
                                } else {
                                    (
                                        super::orchestrator::TREE_MID_BULLET,
                                        super::orchestrator::TREE_MID_DURATION,
                                    )
                                };
                                let pulsed_grey = pulse_color(
                                    Color::Rgb {
                                        r: 128,
                                        g: 128,
                                        b: 128,
                                    },
                                    ticks,
                                );
                                let _ = execute!(stdout, cursor::SavePosition);
                                let _ = execute!(
                                    stdout,
                                    cursor::MoveUp(bullet_up as u16),
                                    cursor::MoveToColumn(0)
                                );
                                let _ = execute!(
                                    stdout,
                                    terminal::Clear(terminal::ClearType::CurrentLine)
                                );
                                print!(
                                    "{}{} {}",
                                    b_prefix.with(Color::DarkGrey),
                                    "●".with(pulsed_grey),
                                    tool.tool_display.as_str().with(Color::White),
                                );
                                let _ = execute!(stdout, cursor::RestorePosition);
                                let _ = execute!(stdout, cursor::SavePosition);
                                let _ = execute!(
                                    stdout,
                                    cursor::MoveUp(duration_up as u16),
                                    cursor::MoveToColumn(0)
                                );
                                let _ = execute!(
                                    stdout,
                                    terminal::Clear(terminal::ClearType::CurrentLine)
                                );
                                let running = format_orch_running(tool.start_time);
                                print!(
                                    "{}{} {}",
                                    d_prefix.with(Color::DarkGrey),
                                    "⎿".with(Color::DarkGrey),
                                    running.as_str().with(Color::DarkGrey),
                                );
                                let _ = execute!(stdout, cursor::RestorePosition);
                            }
                            drop(last_tool_map);
                            let _ = stdout.flush();
                        }

                        tick_queued_wave();

                        if STREAM_PANEL_DIRTY
                            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                        {
                            render_stream_panel_in_place();
                        }
                    }
                    cache_anim_lines(&top, &bottom);
                }

                if !has_thoughts && ticks.is_multiple_of(12) {
                    dot_pos = (dot_pos + 1) % 3;
                }

                if has_thoughts && ticks.is_multiple_of(150) {
                    thought_idx = (thought_idx + 1) % thoughts.len();
                    thought_wave_pos = 0.0;
                }
            }
        });

        (
            Self {
                stop,
                handle: Some(handle),
            },
            stop_for_caller,
        )
    }

    /// Stop the animation, collapse the two lines, leave frame in place.
    pub fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        cache_anim_lines("", "");
        collapse_two_lines_above_frame();
    }
}

impl Drop for WaveAnimation {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Stop the thinking animation via the shared flag and collapse its two lines.
pub fn stop_and_clear_animation(stop_flag: &AtomicBool) {
    if !stop_flag.load(Ordering::Relaxed) {
        stop_flag.store(true, Ordering::Relaxed);
    }
    thread::sleep(Duration::from_millis(60));
    collapse_two_lines_above_frame();
}

// ---------------------------------------------------------------------------
// ToolStatusAnimation (live timer on the └─ line)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct ToolStatusAnimation {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

#[allow(dead_code)]
impl ToolStatusAnimation {
    pub fn start(
        input_buf: Arc<Mutex<String>>,
        cancel: Option<Arc<AtomicBool>>,
    ) -> (Self, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let stop_for_caller = stop.clone();

        let start_time = std::time::Instant::now();
        let bullet_color = random_bullet_color();

        let elapsed = format_duration(start_time.elapsed());
        let bullet_line = format!("{}", "●".with(bullet_color).attribute(Attribute::Bold),);
        let timer_line = format!(
            "{} {} {}",
            "└─".with(Color::DarkGrey),
            "tool started".with(Color::White),
            format!("({elapsed})").with(Color::White),
        );
        {
            let _term = lock_term();
            update_bullet_above_tool_line(&bullet_line);
            update_line_above_frame(&timer_line);
        }
        cache_anim_lines(&bullet_line, &timer_line);

        let handle = thread::spawn(move || {
            let mut ticks: u32 = 0;
            while !stop_clone.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(20));
                if stop_clone.load(Ordering::Relaxed) {
                    return;
                }

                if check_resize().is_some() {
                    let _term = lock_term();
                    let mut stdout = io::stdout();
                    let _ = execute!(
                        stdout,
                        terminal::Clear(terminal::ClearType::All),
                        cursor::MoveTo(0, 0)
                    );
                    let pulsed = pulse_color(bullet_color, ticks);
                    let bl = format!("{}", "●".with(pulsed).attribute(Attribute::Bold));
                    let elapsed = format_duration(start_time.elapsed());
                    let tl = format!(
                        "{} {} {}",
                        "└─".with(Color::DarkGrey),
                        "tool started".with(Color::White),
                        format!("({elapsed})").with(Color::White),
                    );
                    println!("{bl}");
                    println!("{tl}");
                    cache_anim_lines(&bl, &tl);
                    redraw_input_frame();
                    if let Ok(buf) = input_buf.lock() {
                        render_input_line(&buf);
                    }
                    continue;
                }

                if let Ok(mut buf) = input_buf.lock() {
                    let prev_len = buf.len();
                    if drain_stdin(&mut buf)
                        && let Some(ref c) = cancel
                    {
                        c.store(true, Ordering::Relaxed);
                    }
                    if buf.len() != prev_len {
                        let _term = lock_term();
                        render_input_line(&buf);
                    }
                }

                ticks += 1;

                if ticks.is_multiple_of(5) {
                    let pulsed = pulse_color(bullet_color, ticks);
                    let bullet_line = format!("{}", "●".with(pulsed).attribute(Attribute::Bold),);
                    let elapsed = format_duration(start_time.elapsed());
                    let timer_line = format!(
                        "{} {} {}",
                        "└─".with(Color::DarkGrey),
                        "tool started".with(Color::White),
                        format!("({elapsed})").with(Color::White),
                    );
                    {
                        let _term = lock_term();
                        update_bullet_above_tool_line(&bullet_line);
                        update_line_above_frame(&timer_line);
                    }
                    cache_anim_lines(&bullet_line, &timer_line);
                }

                if ticks.is_multiple_of(3) {
                    let _term = lock_term();
                    tick_queued_wave();

                    if STREAM_PANEL_DIRTY
                        .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        render_stream_panel_in_place();
                    }
                }
            }
        });

        (
            Self {
                stop,
                handle: Some(handle),
            },
            stop_for_caller,
        )
    }

    /// Stop the timer, replace with final "tool completed (Xs)".
    pub fn finish(mut self, duration: Duration) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        cache_anim_lines("", "");
        let time_str = format_duration(duration);

        let _term = lock_term();
        erase_input_frame();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, cursor::MoveUp(1), cursor::MoveToColumn(0));
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
        println!(
            "{} {} {}",
            "└─".with(Color::DarkGrey),
            "tool completed".with(Color::White),
            format!("({time_str})").with(Color::White),
        );
        println!();
        redraw_input_frame();
    }
}

impl Drop for ToolStatusAnimation {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Fallback: finish the tool status sub-line when no ToolStatusAnimation ran.
#[allow(dead_code)]
pub fn finish_tool_call_line(duration: Duration) {
    let time_str = format_duration(duration);

    erase_input_frame();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::MoveUp(1), cursor::MoveToColumn(0));
    let _ = execute!(stdout, terminal::Clear(terminal::ClearType::CurrentLine));
    println!(
        "{} {} {}",
        "└─".with(Color::DarkGrey),
        "tool completed".with(Color::White),
        format!("({time_str})").with(Color::White),
    );
    println!();
    redraw_input_frame();
}

/// Print a tool call line + status sub-line, then redraw the frame below.
#[allow(dead_code)]
pub fn print_tool_call_line(
    tool_name: &str,
    args: &std::collections::BTreeMap<String, serde_json::Value>,
) {
    let display = crate::tools::format_tool_call_display_from_args(tool_name, args);

    let (width, _) = term_size();
    let args_display = if display.len() > width as usize {
        let budget = (width as usize).saturating_sub(4);
        format!("{}...", &display[..budget])
    } else {
        display
    };

    erase_input_frame();
    println!(
        "{} {}",
        "●".with(random_bullet_color()).attribute(Attribute::Bold),
        args_display.with(Color::White),
    );
    println!(
        "{} {}",
        "└─".with(Color::DarkGrey),
        "tool requested".with(Color::White),
    );
    redraw_input_frame();
}
