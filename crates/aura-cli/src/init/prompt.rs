//! Interactive prompt helper. All prompts go through here so the resolution
//! logic stays testable without a terminal — tests construct a `Prompter` over
//! scripted stdin.

use std::io::{BufRead, Write};
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Result, bail};
use crossterm::{
    cursor::{MoveLeft, MoveRight},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    queue,
    terminal::{Clear, ClearType},
};
use signal_hook::{
    SigId,
    consts::signal::{SIGINT, SIGTERM},
    low_level,
};
use unicode_width::UnicodeWidthChar;

const OUTSIDE_PROMPT: usize = 0;
const ACTIVE_PROMPT: usize = 1;

fn signal_state(signal: i32) -> usize {
    signal as usize + 1
}

fn state_signal(state: usize) -> Option<usize> {
    (state > ACTIVE_PROMPT).then_some(state - 1)
}

/// Record a signal while a prompt owns the terminal. A `true` return means
/// teardown already won the atomic transition and the default action must run.
fn record_prompt_signal(state: &AtomicUsize, signal: i32) -> bool {
    state
        .compare_exchange(
            ACTIVE_PROMPT,
            signal_state(signal),
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err_and(|current| current == OUTSIDE_PROMPT)
}

fn is_prompt_interrupt(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'z' | 'Z'))
}

struct PromptSignals {
    state: Arc<AtomicUsize>,
    _signal_ids: Vec<SigId>,
}

impl PromptSignals {
    fn install() -> std::io::Result<Self> {
        let state = Arc::new(AtomicUsize::new(OUTSIDE_PROMPT));
        let mut signal_ids = Vec::new();
        for signal in [SIGINT, SIGTERM] {
            // Keep the signal-hook actions for the process lifetime. Removing
            // the final action would leave the signal ignored rather than
            // restoring its default disposition.
            let state = Arc::clone(&state);
            signal_ids.push(unsafe {
                // SAFETY: the handler performs only atomic operations and
                // signal-hook's signal-safe default-action emulation.
                low_level::register(signal, move || {
                    if record_prompt_signal(&state, signal) {
                        let _ = low_level::emulate_default_handler(signal);
                    }
                })?
            });
        }
        Ok(Self {
            state,
            _signal_ids: signal_ids,
        })
    }

    fn finish_prompt(&self) -> Option<usize> {
        // The swap linearizes teardown with the signal handler's compare-and-
        // exchange. A signal is therefore either returned here or observes the
        // outside state and takes its default action; it cannot be swallowed.
        state_signal(self.state.swap(OUTSIDE_PROMPT, Ordering::SeqCst))
    }
}

static PROMPT_SIGNALS: LazyLock<std::io::Result<PromptSignals>> =
    LazyLock::new(PromptSignals::install);

struct RawModeGuard {
    signals: &'static PromptSignals,
    active: bool,
}

impl RawModeGuard {
    fn enter() -> std::io::Result<Self> {
        let signals = PROMPT_SIGNALS
            .as_ref()
            .map_err(|error| std::io::Error::new(error.kind(), error.to_string()))?;
        signals.state.store(ACTIVE_PROMPT, Ordering::SeqCst);
        if let Err(error) = crossterm::terminal::enable_raw_mode() {
            signals.finish_prompt();
            return Err(error);
        }
        Ok(Self {
            signals,
            active: true,
        })
    }

    fn pending_signal(&self) -> Option<usize> {
        state_signal(self.signals.state.load(Ordering::SeqCst))
    }

    fn restore(&mut self) -> Option<usize> {
        if !self.active {
            return None;
        }
        let _ = crossterm::terminal::disable_raw_mode();
        self.active = false;
        self.signals.finish_prompt()
    }

    fn finish<T>(mut self, value: T) -> Result<T> {
        if let Some(signal) = self.restore() {
            bail!("init interrupted by signal {signal}");
        }
        Ok(value)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

#[derive(Default)]
struct LineEditor {
    chars: Vec<char>,
    cursor: usize,
}

impl LineEditor {
    fn text(&self) -> String {
        self.chars.iter().collect()
    }

    fn cursor_width(&self) -> usize {
        display_width(&self.chars[..self.cursor])
    }

    fn insert(&mut self, value: char) {
        self.chars.insert(self.cursor, value);
        self.cursor += 1;
    }

    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        self.chars.remove(self.cursor);
        true
    }

    fn delete(&mut self) -> bool {
        if self.cursor == self.chars.len() {
            return false;
        }
        self.chars.remove(self.cursor);
        true
    }

    fn clear_to_start(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.chars.drain(..self.cursor);
        self.cursor = 0;
        true
    }

    fn erase_word(&mut self) -> bool {
        let initial = self.cursor;
        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        if self.cursor == initial {
            return false;
        }
        self.chars.drain(self.cursor..initial);
        true
    }

    fn redraw(&self, old_cursor_width: usize, echo: bool) -> std::io::Result<()> {
        if !echo {
            return Ok(());
        }
        let mut stdout = std::io::stdout();
        queue_move_left(&mut stdout, old_cursor_width)?;
        queue!(stdout, Clear(ClearType::UntilNewLine))?;
        let text = self.text();
        write!(stdout, "{text}")?;
        queue_move_left(
            &mut stdout,
            display_width(&self.chars).saturating_sub(self.cursor_width()),
        )?;
        stdout.flush()
    }
}

fn display_width(chars: &[char]) -> usize {
    chars
        .iter()
        .map(|character| character.width().unwrap_or(0))
        .sum()
}

fn queue_move_left(output: &mut impl Write, mut columns: usize) -> std::io::Result<()> {
    while columns > 0 {
        let step = columns.min(u16::MAX as usize) as u16;
        queue!(output, MoveLeft(step))?;
        columns -= usize::from(step);
    }
    Ok(())
}

fn queue_move_right(output: &mut impl Write, mut columns: usize) -> std::io::Result<()> {
    while columns > 0 {
        let step = columns.min(u16::MAX as usize) as u16;
        queue!(output, MoveRight(step))?;
        columns -= usize::from(step);
    }
    Ok(())
}

fn read_tty_answer(echo: bool) -> Result<Option<String>> {
    let raw_mode = RawModeGuard::enter()?;
    let mut editor = LineEditor::default();

    loop {
        if let Some(signal) = raw_mode.pending_signal() {
            bail!("init interrupted by signal {signal}");
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if is_prompt_interrupt(&key) {
            print!(
                "^{}\r\n",
                match key.code {
                    KeyCode::Char(c) => c.to_ascii_uppercase(),
                    _ => unreachable!(),
                }
            );
            std::io::stdout().flush()?;
            bail!("init interrupted");
        }

        match key.code {
            KeyCode::Enter => {
                print!("\r\n");
                std::io::stdout().flush()?;
                return raw_mode.finish(Some(editor.text()));
            }
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && editor.chars.is_empty() =>
            {
                print!("\r\n");
                std::io::stdout().flush()?;
                return raw_mode.finish(None);
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let columns = editor.cursor_width();
                editor.cursor = 0;
                if echo {
                    let mut stdout = std::io::stdout();
                    queue_move_left(&mut stdout, columns)?;
                    stdout.flush()?;
                }
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let columns = display_width(&editor.chars[editor.cursor..]);
                editor.cursor = editor.chars.len();
                if echo {
                    let mut stdout = std::io::stdout();
                    queue_move_right(&mut stdout, columns)?;
                    stdout.flush()?;
                }
            }
            KeyCode::Char('b')
                if key.modifiers.contains(KeyModifiers::CONTROL) && editor.cursor > 0 =>
            {
                let columns = editor.chars[editor.cursor - 1].width().unwrap_or(0);
                editor.cursor -= 1;
                if echo {
                    let mut stdout = std::io::stdout();
                    queue_move_left(&mut stdout, columns)?;
                    stdout.flush()?;
                }
            }
            KeyCode::Char('f')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && editor.cursor < editor.chars.len() =>
            {
                let columns = editor.chars[editor.cursor].width().unwrap_or(0);
                editor.cursor += 1;
                if echo {
                    let mut stdout = std::io::stdout();
                    queue_move_right(&mut stdout, columns)?;
                    stdout.flush()?;
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let old_cursor_width = editor.cursor_width();
                if editor.clear_to_start() {
                    editor.redraw(old_cursor_width, echo)?;
                }
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let old_cursor_width = editor.cursor_width();
                if editor.erase_word() {
                    editor.redraw(old_cursor_width, echo)?;
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let old_cursor_width = editor.cursor_width();
                editor.insert(c);
                editor.redraw(old_cursor_width, echo)?;
            }
            KeyCode::Backspace => {
                let old_cursor_width = editor.cursor_width();
                if editor.backspace() {
                    editor.redraw(old_cursor_width, echo)?;
                }
            }
            KeyCode::Delete => {
                let old_cursor_width = editor.cursor_width();
                if editor.delete() {
                    editor.redraw(old_cursor_width, echo)?;
                }
            }
            KeyCode::Left if editor.cursor > 0 => {
                let columns = editor.chars[editor.cursor - 1].width().unwrap_or(0);
                editor.cursor -= 1;
                if echo {
                    let mut stdout = std::io::stdout();
                    queue_move_left(&mut stdout, columns)?;
                    stdout.flush()?;
                }
            }
            KeyCode::Right if editor.cursor < editor.chars.len() => {
                let columns = editor.chars[editor.cursor].width().unwrap_or(0);
                editor.cursor += 1;
                if echo {
                    let mut stdout = std::io::stdout();
                    queue_move_right(&mut stdout, columns)?;
                    stdout.flush()?;
                }
            }
            KeyCode::Home => {
                let columns = editor.cursor_width();
                editor.cursor = 0;
                if echo {
                    let mut stdout = std::io::stdout();
                    queue_move_left(&mut stdout, columns)?;
                    stdout.flush()?;
                }
            }
            KeyCode::End => {
                let columns = display_width(&editor.chars[editor.cursor..]);
                editor.cursor = editor.chars.len();
                if echo {
                    let mut stdout = std::io::stdout();
                    queue_move_right(&mut stdout, columns)?;
                    stdout.flush()?;
                }
            }
            _ => {}
        }
    }
}

/// Interactive prompt helper. Fields are `pub(crate)` so tests in sibling
/// modules can build one over scripted input.
pub(crate) struct Prompter<R: BufRead> {
    pub(crate) interactive: bool,
    /// True when reading from a real terminal. Secret prompts then read with
    /// echo suppressed; in tests (scripted stdin) this is false so `ask_secret`
    /// reads the injected `stdin` instead.
    pub(crate) is_tty: bool,
    pub(crate) stdin: R,
}

impl<R: BufRead> Prompter<R> {
    /// Read one answer while preserving terminal interrupt semantics.
    ///
    /// `BufRead::read_line` cannot distinguish Ctrl-C or Ctrl-Z from text when
    /// the terminal does not generate signals. The TTY reader handles those
    /// keys directly so setup exits without waiting for Enter.
    fn read_answer(&mut self) -> Result<Option<String>> {
        if self.is_tty {
            return read_tty_answer(true);
        }

        let mut line = String::new();
        if self.stdin.read_line(&mut line)? == 0 {
            Ok(None)
        } else {
            Ok(Some(line))
        }
    }

    /// Ask a question with an optional default. Returns `None` when
    /// non-interactive (the caller decides whether that's fatal).
    pub(crate) fn ask(&mut self, question: &str, default: Option<&str>) -> Result<Option<String>> {
        if !self.interactive {
            return Ok(default.map(String::from));
        }
        match default {
            Some(d) => print!("{question} [{d}]: "),
            None => print!("{question}: "),
        }
        std::io::stdout().flush()?;
        let line = self.read_answer()?.unwrap_or_default();
        let answer = line.trim();
        if answer.is_empty() {
            Ok(default.map(String::from))
        } else {
            Ok(Some(answer.to_string()))
        }
    }

    /// Ask with no default; in non-interactive mode a missing value is an
    /// error naming the flag that would have provided it.
    pub(crate) fn require(&mut self, question: &str, flag: &str) -> Result<String> {
        match self.ask(question, None)? {
            Some(v) => Ok(v),
            None => bail!("{flag} is required in non-interactive mode"),
        }
    }

    /// Ask the operator to pick one of `n` numbered choices (1-based on
    /// screen), returning the 0-based index. Only a number in `1..=n` is
    /// accepted — anything else re-prompts. An empty line accepts `default`
    /// when one is given (otherwise it too re-prompts). Returns `None` in
    /// non-interactive mode (the caller decides whether that's fatal) and on
    /// EOF, so scripted/exhausted input terminates instead of looping.
    pub(crate) fn ask_choice(
        &mut self,
        question: &str,
        n: usize,
        default: Option<usize>,
    ) -> Result<Option<usize>> {
        if !self.interactive {
            return Ok(default);
        }
        loop {
            match default {
                Some(d) => print!("{question} [{}]: ", d + 1),
                None => print!("{question}: "),
            }
            std::io::stdout().flush()?;
            let Some(line) = self.read_answer()? else {
                return Ok(default);
            };
            let answer = line.trim();
            if answer.is_empty() {
                if default.is_some() {
                    return Ok(default);
                }
            } else if let Ok(i) = answer.parse::<usize>()
                && (1..=n).contains(&i)
            {
                return Ok(Some(i - 1));
            }
            eprintln!("Please enter a number between 1 and {n}.");
        }
    }

    /// Ask a yes/no question. Returns the default in non-interactive mode.
    pub(crate) fn ask_yes_no(&mut self, question: &str, default: bool) -> Result<bool> {
        if !self.interactive {
            return Ok(default);
        }
        let hint = if default { "Y/n" } else { "y/N" };
        print!("{question} [{hint}]: ");
        std::io::stdout().flush()?;
        let line = self.read_answer()?.unwrap_or_default();
        let answer = line.trim().to_lowercase();
        if answer.is_empty() {
            Ok(default)
        } else {
            Ok(answer.starts_with('y'))
        }
    }

    /// Prompt for an API key with masked input. On a real terminal the shared
    /// interrupt-aware reader suppresses echo. In test contexts (`is_tty =
    /// false`), input comes from the injected reader. Returns `None` on empty
    /// input or EOF.
    pub(crate) fn ask_secret_masked(&mut self, prompt: &str) -> Result<Option<String>> {
        if !self.interactive {
            return Ok(None);
        }
        print!("{prompt}: ");
        std::io::stdout().flush()?;
        let raw = if self.is_tty {
            read_tty_answer(false)?.unwrap_or_default()
        } else {
            let mut line = String::new();
            self.stdin.read_line(&mut line)?;
            line
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    /// Pick a model: a number selects from the displayed `shortlist`, an empty
    /// line / EOF accepts the suggested entry, and anything else is taken as a
    /// typed model id verbatim. Out-of-range numbers re-prompt. Returns the
    /// suggested entry in non-interactive mode (`None` if the shortlist is
    /// empty, which the caller turns into the `--model` requirement).
    pub(crate) fn ask_model(
        &mut self,
        shortlist: &[String],
        suggested_index: usize,
    ) -> Result<Option<String>> {
        let suggested = shortlist.get(suggested_index).cloned();
        if !self.interactive {
            return Ok(suggested);
        }
        loop {
            match &suggested {
                Some(d) => print!("Which model should AURA use? [{d}]: "),
                None => print!("Which model should AURA use?: "),
            }
            std::io::stdout().flush()?;
            let Some(line) = self.read_answer()? else {
                return Ok(suggested);
            };
            let answer = line.trim();
            if answer.is_empty() {
                return Ok(suggested);
            }
            if !shortlist.is_empty()
                && let Ok(n) = answer.parse::<usize>()
            {
                if (1..=shortlist.len()).contains(&n) {
                    return Ok(Some(shortlist[n - 1].clone()));
                }
                eprintln!(
                    "Please enter a number between 1 and {}, or a model id.",
                    shortlist.len()
                );
                continue;
            }
            return Ok(Some(answer.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use signal_hook::consts::signal::SIGTERM;

    use crate::init::test_support::{non_interactive, scripted};

    use super::{
        ACTIVE_PROMPT, LineEditor, OUTSIDE_PROMPT, PromptSignals, is_prompt_interrupt,
        record_prompt_signal,
    };

    fn sample_shortlist() -> Vec<String> {
        vec![
            "gpt-5.6".to_string(),
            "gpt-4.1".to_string(),
            "gpt-4o".to_string(),
        ]
    }

    #[test]
    fn tty_interrupt_keys_exit_init() {
        for key in ['c', 'C', 'z', 'Z'] {
            assert!(is_prompt_interrupt(&KeyEvent::new(
                KeyCode::Char(key),
                KeyModifiers::CONTROL,
            )));
        }
        assert!(!is_prompt_interrupt(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn prompt_signal_and_teardown_have_no_unhandled_order() {
        let signals = PromptSignals {
            state: Arc::new(AtomicUsize::new(ACTIVE_PROMPT)),
            _signal_ids: Vec::new(),
        };

        // If the handler wins, teardown consumes the pending signal.
        assert!(!record_prompt_signal(&signals.state, SIGTERM));
        assert_eq!(signals.finish_prompt(), Some(SIGTERM as usize));

        // If teardown wins, the handler observes that it must run the signal's
        // default action instead of leaving an unconsumed pending value.
        signals.state.store(ACTIVE_PROMPT, Ordering::SeqCst);
        assert_eq!(signals.finish_prompt(), None);
        assert!(record_prompt_signal(&signals.state, SIGTERM));
        assert_eq!(signals.state.load(Ordering::SeqCst), OUTSIDE_PROMPT);
    }

    #[test]
    fn tty_line_editor_preserves_clear_and_word_erase() {
        let mut editor = LineEditor::default();
        for character in "first second  ".chars() {
            editor.insert(character);
        }
        assert!(editor.erase_word());
        assert_eq!(editor.text(), "first ");
        assert!(editor.clear_to_start());
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn tty_line_editor_supports_cursor_insert_and_delete() {
        let mut editor = LineEditor::default();
        for character in "ac".chars() {
            editor.insert(character);
        }
        editor.cursor = 1;
        editor.insert('b');
        assert_eq!(editor.text(), "abc");
        assert!(editor.backspace());
        assert_eq!(editor.text(), "ac");
        assert!(editor.delete());
        assert_eq!(editor.text(), "a");
    }

    #[test]
    fn ask_model_empty_uses_suggested() {
        let mut p = scripted("\n");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("gpt-5.6".to_string())
        );
    }

    #[test]
    fn ask_model_eof_uses_suggested() {
        let mut p = scripted("");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("gpt-5.6".to_string())
        );
    }

    #[test]
    fn ask_model_number_selects_from_shortlist() {
        let mut p = scripted("2\n");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("gpt-4.1".to_string())
        );
    }

    #[test]
    fn ask_model_typed_id_is_used_verbatim() {
        let mut p = scripted("my-finetune\n");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("my-finetune".to_string())
        );
    }

    #[test]
    fn ask_model_out_of_range_number_reprompts_then_typed() {
        let mut p = scripted("9\nmy-ft\n");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("my-ft".to_string())
        );
    }

    #[test]
    fn ask_model_non_interactive_returns_suggested() {
        assert_eq!(
            non_interactive().ask_model(&sample_shortlist(), 0).unwrap(),
            Some("gpt-5.6".to_string())
        );
        assert_eq!(non_interactive().ask_model(&[], 0).unwrap(), None);
    }

    #[test]
    fn ask_choice_rejects_until_valid_number() {
        let mut p = scripted("9\n0\nfoo\n2\n");
        assert_eq!(p.ask_choice("Provider", 6, None).unwrap(), Some(1));
    }

    #[test]
    fn ask_choice_empty_uses_default() {
        let mut p = scripted("\n");
        assert_eq!(p.ask_choice("Provider", 6, Some(3)).unwrap(), Some(3));
    }

    #[test]
    fn ask_choice_non_interactive_returns_default() {
        assert_eq!(
            non_interactive()
                .ask_choice("Provider", 6, Some(2))
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            non_interactive().ask_choice("Provider", 6, None).unwrap(),
            None
        );
    }

    #[test]
    fn ask_yes_no_defaults() {
        let mut p = scripted("\n");
        assert!(p.ask_yes_no("test?", true).unwrap());
        let mut p = scripted("\n");
        assert!(!p.ask_yes_no("test?", false).unwrap());
    }

    #[test]
    fn ask_yes_no_explicit() {
        let mut p = scripted("y\n");
        assert!(p.ask_yes_no("test?", false).unwrap());
        let mut p = scripted("n\n");
        assert!(!p.ask_yes_no("test?", true).unwrap());
    }
}
