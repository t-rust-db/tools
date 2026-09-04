//! The generic REPL loop, parameterized over a [`ReplHandler`] that knows
//! how to execute and format one engine's queries. Dispatch logic (dot-command
//! handling, statement buffering, mode switching) lives in [`Repl`], which is
//! decoupled from terminal I/O so it can be unit tested by feeding it plain
//! strings.

use std::io;
use std::path::Path;

use crate::editor::{Readline, ReadlineError};
use crate::output::OutputMode;

/// Implemented by each engine's CLI to plug its query execution and result
/// formatting into the generic REPL loop.
///
/// `Output` is left up to the handler: a simple engine can use
/// `(Vec<String>, Vec<Vec<String>>)` and format it with
/// [`crate::output::render`]; an engine with richer result shapes (e.g. an
/// `EXPLAIN` plan tree) can use its own enum and format each variant however
/// it likes — `db-cli` never needs to know the engine's types.
pub trait ReplHandler {
    type Output;

    /// Execute one statement (buffered up to a trailing `;`).
    fn execute(&mut self, input: &str) -> Result<Self::Output, String>;

    /// Render a previously-executed result under the given mode.
    fn format(&self, output: &Self::Output, mode: OutputMode) -> String;

    /// Handle a dot-command (`name` without the leading `.`, plus any
    /// trailing argument text). Return `Some(lines to print)` if handled,
    /// `None` if the command is unrecognized. The generic commands `.help`,
    /// `.quit`/`.exit`, `.mode`, and `.color` are handled by [`Repl`] itself
    /// and never reach this method.
    fn command(&mut self, name: &str, arg: &str) -> Option<Vec<String>> {
        let _ = (name, arg);
        None
    }

    /// Extra lines appended to the built-in `.help` output.
    fn help_extra(&self) -> Vec<String> {
        Vec::new()
    }

    /// Printed once at REPL startup.
    fn banner(&self) -> Option<String> {
        None
    }
}

/// One step of driving the REPL with a single input line.
pub enum Step {
    /// Nothing to print yet — still buffering a statement, or a dot-command
    /// with no output.
    Continue,
    /// Print this text (a result, an error, or a command's output).
    Print(String),
    /// Exit the REPL loop.
    Quit,
}

/// I/O-free REPL dispatch: statement buffering, dot-commands, and mode
/// switching. Exposed separately from [`run_repl`] so tests can drive it
/// with plain strings instead of a real terminal.
pub struct Repl<H: ReplHandler> {
    handler: H,
    mode: OutputMode,
    buffer: String,
}

impl<H: ReplHandler> Repl<H> {
    pub fn new(handler: H) -> Self {
        Repl {
            handler,
            mode: OutputMode::Table,
            buffer: String::new(),
        }
    }

    pub fn mode(&self) -> OutputMode {
        self.mode
    }

    /// True while a multi-line statement is still being buffered (i.e. the
    /// prompt should switch to a continuation prompt).
    pub fn is_buffering(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Feed one line of input (already read from the terminal or a pipe).
    pub fn feed_line(&mut self, line: &str) -> Step {
        if self.buffer.is_empty() {
            if let Some(rest) = line.trim().strip_prefix('.') {
                return self.dot_command(rest);
            }
        }

        if !self.buffer.is_empty() {
            self.buffer.push(' ');
        }
        self.buffer.push_str(line.trim());

        if self.buffer.trim_end().ends_with(';') {
            let stmt = self.buffer.trim_end_matches(';').trim().to_string();
            self.buffer.clear();
            if stmt.is_empty() {
                return Step::Continue;
            }
            return match self.handler.execute(&stmt) {
                Ok(output) => Step::Print(self.handler.format(&output, self.mode)),
                Err(e) => Step::Print(format!("error: {e}")),
            };
        }

        Step::Continue
    }

    fn dot_command(&mut self, rest: &str) -> Step {
        let cmd = rest.split_whitespace().next().unwrap_or("");
        let arg = rest
            .split_once(char::is_whitespace)
            .map(|(_, a)| a.trim())
            .unwrap_or("");

        if !cmd.is_empty() && ("quit".starts_with(cmd) || "exit".starts_with(cmd)) {
            return Step::Quit;
        }
        if !cmd.is_empty() && "help".starts_with(cmd) {
            let mut lines = vec![
                ".help          Show this message".to_string(),
                ".mode <table|json|csv>  Set output format".to_string(),
                ".quit / .exit  Exit".to_string(),
            ];
            lines.extend(self.handler.help_extra());
            lines.push(String::new());
            lines.push("Keybindings:".to_string());
            lines.push("  Ctrl-A / Ctrl-E   line start / end".to_string());
            lines.push("  Ctrl-B / Ctrl-F   char left / right".to_string());
            lines.push("  Ctrl-K / Ctrl-U   kill to end / start of line".to_string());
            lines.push("  Ctrl-W            kill previous word".to_string());
            lines.push("  Alt-B / Alt-F     word left / right".to_string());
            lines.push("  Ctrl-L            clear screen".to_string());
            lines.push("  Ctrl-P / Ctrl-N   history previous / next".to_string());
            return Step::Print(lines.join("\n"));
        }
        if !cmd.is_empty() && "mode".starts_with(cmd) {
            return match OutputMode::parse(arg) {
                Some(m) => {
                    self.mode = m;
                    Step::Print(format!("mode: {arg}"))
                }
                None => Step::Print(".mode table|json|csv".to_string()),
            };
        }

        match self.handler.command(cmd, arg) {
            Some(lines) => Step::Print(lines.join("\n")),
            None => Step::Print(format!("unknown command: .{cmd}")),
        }
    }

    pub fn into_handler(self) -> H {
        self.handler
    }

    pub fn handler_mut(&mut self) -> &mut H {
        &mut self.handler
    }
}

/// Options for [`run_repl`].
pub struct ReplOptions<'a> {
    pub prompt: &'a str,
    pub continuation_prompt: &'a str,
    pub history_file: Option<&'a Path>,
}

impl<'a> Default for ReplOptions<'a> {
    fn default() -> Self {
        ReplOptions {
            prompt: "> ",
            continuation_prompt: "  -> ",
            history_file: None,
        }
    }
}

/// Run the interactive REPL loop against `handler` until EOF or `.quit`.
pub fn run_repl<H: ReplHandler>(handler: H, opts: ReplOptions) -> io::Result<()> {
    let mut repl = Repl::new(handler);
    let mut editor = Readline::new();

    if let Some(hf) = opts.history_file {
        editor.load_history(hf);
    }

    if let Some(banner) = repl.handler_mut().banner() {
        println!("{banner}");
    }

    loop {
        let prompt = if repl.is_buffering() {
            opts.continuation_prompt
        } else {
            opts.prompt
        };
        let line = match editor.read_line(prompt) {
            Ok(l) => l,
            Err(ReadlineError::Eof) => break,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Io(e)) => return Err(e),
        };

        if !line.trim().is_empty() {
            editor.add_history_entry(&line);
        }

        match repl.feed_line(&line) {
            Step::Continue => {}
            Step::Print(text) => println!("{text}"),
            Step::Quit => break,
        }
    }

    if let Some(hf) = opts.history_file {
        editor.save_history(hf);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockHandler {
        executed: Vec<String>,
    }

    impl ReplHandler for MockHandler {
        type Output = (Vec<String>, Vec<Vec<String>>);

        fn execute(&mut self, input: &str) -> Result<Self::Output, String> {
            self.executed.push(input.to_string());
            if input == "fail" {
                return Err("boom".to_string());
            }
            Ok((vec!["x".to_string()], vec![vec![input.to_string()]]))
        }

        fn format(&self, output: &Self::Output, mode: OutputMode) -> String {
            crate::output::render(mode, &output.0, &output.1)
        }

        fn command(&mut self, name: &str, arg: &str) -> Option<Vec<String>> {
            if name == "echo" {
                Some(vec![arg.to_string()])
            } else {
                None
            }
        }
    }

    fn mock() -> Repl<MockHandler> {
        Repl::new(MockHandler {
            executed: Vec::new(),
        })
    }

    #[test]
    fn buffers_until_semicolon() {
        let mut repl = mock();
        assert!(matches!(repl.feed_line("select"), Step::Continue));
        assert!(repl.is_buffering());
        assert!(matches!(repl.feed_line("1;"), Step::Print(_)));
        assert!(!repl.is_buffering());
        assert_eq!(repl.into_handler().executed, vec!["select 1".to_string()]);
    }

    #[test]
    fn execute_error_is_printed_not_fatal() {
        let mut repl = mock();
        match repl.feed_line("fail;") {
            Step::Print(text) => assert!(text.contains("error: boom")),
            _ => panic!("expected Print"),
        }
    }

    #[test]
    fn quit_and_exit_dot_commands_stop_the_repl() {
        assert!(matches!(mock().feed_line(".quit"), Step::Quit));
        assert!(matches!(mock().feed_line(".exit"), Step::Quit));
    }

    #[test]
    fn mode_command_switches_output_mode() {
        let mut repl = mock();
        repl.feed_line(".mode json");
        assert_eq!(repl.mode(), OutputMode::Json);
    }

    #[test]
    fn unknown_dot_command_reports_itself() {
        match mock().feed_line(".bogus") {
            Step::Print(text) => assert_eq!(text, "unknown command: .bogus"),
            _ => panic!("expected Print"),
        }
    }

    #[test]
    fn handler_command_is_dispatched() {
        match mock().feed_line(".echo hello") {
            Step::Print(text) => assert_eq!(text, "hello"),
            _ => panic!("expected Print"),
        }
    }

    #[test]
    fn empty_statement_is_ignored() {
        assert!(matches!(mock().feed_line(";"), Step::Continue));
    }
}
