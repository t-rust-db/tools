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
    /// The engine's native result shape for one statement.
    type Output;

    /// Execute one statement (buffered up to a trailing `;`).
    fn execute(&mut self, input: &str) -> Result<Self::Output, String>;

    /// Render a previously-executed result under the given mode.
    /// `headers` matches `sqlite3`'s `.headers` toggle (see
    /// [`crate::output::render`]'s per-mode handling of it).
    fn format(&self, output: &Self::Output, mode: OutputMode, headers: bool) -> String;

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
    /// `.headers on|off` -- `false` by default, matching stock `sqlite3`.
    headers: bool,
    /// `.color on|off` -- `run_repl`'s loop syncs this to the `Readline`
    /// it owns after every line, since `Repl` itself (unit-testable with
    /// plain strings) never touches a real `Readline`.
    color: bool,
    buffer: String,
}

impl<H: ReplHandler> Repl<H> {
    /// A REPL around `handler` in `Table` mode, headers off, color on.
    pub fn new(handler: H) -> Self {
        Repl {
            handler,
            mode: OutputMode::Table,
            headers: false,
            color: true,
            buffer: String::new(),
        }
    }

    /// The current output mode (`.mode`).
    pub fn mode(&self) -> OutputMode {
        self.mode
    }

    /// Current `.headers` setting.
    pub fn headers(&self) -> bool {
        self.headers
    }

    /// Current `.color` setting -- `run_repl` reads this each loop
    /// iteration to keep the `Readline` it owns in sync.
    pub fn color(&self) -> bool {
        self.color
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
                Ok(output) => Step::Print(self.handler.format(&output, self.mode, self.headers)),
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
                ".mode <table|list|column|line|csv|json>  Set output format".to_string(),
                ".headers on|off  Toggle header row (list/column/csv)".to_string(),
                ".color on|off  Toggle syntax highlighting".to_string(),
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
                None => Step::Print(".mode table|list|column|line|csv|json".to_string()),
            };
        }
        if !cmd.is_empty() && "headers".starts_with(cmd) {
            return match arg {
                "on" => {
                    self.headers = true;
                    Step::Continue
                }
                "off" => {
                    self.headers = false;
                    Step::Continue
                }
                _ => Step::Print("usage: .headers on|off".to_string()),
            };
        }
        if !cmd.is_empty() && "color".starts_with(cmd) {
            return match arg {
                "on" => {
                    self.color = true;
                    Step::Continue
                }
                "off" => {
                    self.color = false;
                    Step::Continue
                }
                _ => Step::Print("usage: .color on|off".to_string()),
            };
        }

        match self.handler.command(cmd, arg) {
            Some(lines) => Step::Print(lines.join("\n")),
            None => Step::Print(format!("unknown command: .{cmd}")),
        }
    }

    /// Consumes the REPL, returning the engine handler.
    pub fn into_handler(self) -> H {
        self.handler
    }

    /// Mutable access to the engine handler.
    pub fn handler_mut(&mut self) -> &mut H {
        &mut self.handler
    }
}

/// Options for [`run_repl`].
pub struct ReplOptions<'a> {
    /// Prompt shown for the first line of a statement.
    pub prompt: &'a str,
    /// Prompt shown while a statement is still missing its `;`.
    pub continuation_prompt: &'a str,
    /// History file to load on start and save on exit; `None` disables.
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

        editor.set_color(repl.color());
    }

    if let Some(hf) = opts.history_file {
        editor.save_history(hf);
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
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

        fn format(&self, output: &Self::Output, mode: OutputMode, headers: bool) -> String {
            crate::output::render(mode, &output.0, &output.1, headers)
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

    #[test]
    fn mode_command_accepts_list_column_line() {
        let mut repl = mock();
        repl.feed_line(".mode list");
        assert_eq!(repl.mode(), OutputMode::List);
        repl.feed_line(".mode column");
        assert_eq!(repl.mode(), OutputMode::Column);
        repl.feed_line(".mode line");
        assert_eq!(repl.mode(), OutputMode::Line);
    }

    #[test]
    fn headers_default_off_and_toggled_by_dot_command() {
        let mut repl = mock();
        assert!(!repl.headers());
        assert!(matches!(repl.feed_line(".headers on"), Step::Continue));
        assert!(repl.headers());
        assert!(matches!(repl.feed_line(".headers off"), Step::Continue));
        assert!(!repl.headers());
    }

    #[test]
    fn headers_command_rejects_bad_argument() {
        match mock().feed_line(".headers bogus") {
            Step::Print(text) => assert_eq!(text, "usage: .headers on|off"),
            _ => panic!("expected Print"),
        }
    }

    #[test]
    fn headers_setting_reaches_format_via_execute() {
        let mut repl = mock();
        repl.feed_line(".mode list");
        repl.feed_line(".headers on");
        match repl.feed_line("select 1;") {
            Step::Print(text) => assert!(text.starts_with("x\n")),
            _ => panic!("expected Print"),
        }
    }

    #[test]
    fn color_default_on_and_toggled_by_dot_command() {
        let mut repl = mock();
        assert!(repl.color());
        assert!(matches!(repl.feed_line(".color off"), Step::Continue));
        assert!(!repl.color());
        assert!(matches!(repl.feed_line(".color on"), Step::Continue));
        assert!(repl.color());
    }

    #[test]
    fn color_command_rejects_bad_argument() {
        match mock().feed_line(".color bogus") {
            Step::Print(text) => assert_eq!(text, "usage: .color on|off"),
            _ => panic!("expected Print"),
        }
    }
}
