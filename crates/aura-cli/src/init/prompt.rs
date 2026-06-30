//! Interactive prompt helper. All prompts go through here so the resolution
//! logic stays testable without a terminal — tests construct a `Prompter` over
//! scripted stdin.

use std::io::{BufRead, Write as _};

use anyhow::{Result, bail};

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
        let mut line = String::new();
        self.stdin.read_line(&mut line)?;
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
            let mut line = String::new();
            if self.stdin.read_line(&mut line)? == 0 {
                return Ok(default);
            }
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
        let mut line = String::new();
        self.stdin.read_line(&mut line)?;
        let answer = line.trim().to_lowercase();
        if answer.is_empty() {
            Ok(default)
        } else {
            Ok(answer.starts_with('y'))
        }
    }

    /// Prompt for an API key with masked input. On a real terminal the input
    /// is read with echo suppressed via `rpassword`. In test contexts
    /// (`is_tty = false`), falls back to `read_line` on the injected stdin.
    /// Returns `None` on empty input or EOF.
    pub(crate) fn ask_secret_masked(&mut self, prompt: &str) -> Result<Option<String>> {
        if !self.interactive {
            return Ok(None);
        }
        print!("{prompt}: ");
        std::io::stdout().flush()?;
        let raw = if self.is_tty {
            let secret = rpassword::read_password()?;
            println!();
            secret
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
            let mut line = String::new();
            if self.stdin.read_line(&mut line)? == 0 {
                return Ok(suggested);
            }
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
    use crate::init::test_support::{non_interactive, scripted};

    fn sample_shortlist() -> Vec<String> {
        vec![
            "gpt-5.6".to_string(),
            "gpt-4.1".to_string(),
            "gpt-4o".to_string(),
        ]
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
