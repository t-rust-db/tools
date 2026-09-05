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
    /// trailing argument text). Return `Some(lines to print)` if handled
    /// (an empty list means "handled, nothing to print" — e.g. the handler
    /// wrote to stdout itself), `None` if the command is unrecognized. The generic commands `.help`,
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

    /// Whether the buffered input is a complete statement (or several)
    /// ready to run. The default is `sqlite3`'s rule of thumb — a trailing
    /// `;` — but an engine with a tokenizer should override it so a `;`
    /// inside a string literal never ends a statement early (#10).
    fn is_complete(&self, buffer: &str) -> bool {
        buffer.trim_end().ends_with(';')
    }

    /// Splits a complete buffer into the statements to run, in order. The
    /// default hands over one statement with its trailing `;` stripped;
    /// an engine can split `BEGIN; INSERT …;` into two (#10). Empty
    /// results are skipped.
    fn statements(&self, buffer: &str) -> Vec<String> {
        let stmt = buffer.trim().trim_end_matches(';').trim();
        if stmt.is_empty() {
            Vec::new()
        } else {
            vec![stmt.to_string()]
        }
    }

    /// How an `execute` error is rendered before going to stderr. The
    /// default is `error: <message>`; `sqlite3`-style engines override
    /// with `Error: <message>` (#10).
    fn error_line(&self, message: &str) -> String {
        format!("error: {message}")
    }
}

/// One step of driving the REPL with a single input line.
pub enum Step {
    /// Nothing to print yet — still buffering a statement, or a dot-command
    /// with no output.
    Continue,
    /// Print this text to stdout (a result or a command's output).
    Print(String),
    /// Print this text to stderr (an `execute` error, an unknown
    /// dot-command, a usage message) — `sqlite3` keeps errors off stdout
    /// so piped output stays clean (#10).
    Error(String),
    /// Several steps from one input line (a line holding more than one
    /// statement), to be applied in order (#10).
    Many(Vec<Step>),
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

    /// Sets the output mode — for a caller that wants a different start
    /// state than [`Repl::new`]'s `Table` (e.g. `sqlite3`'s `list`), then
    /// drives the loop with [`run_repl_with`] (#10).
    pub fn set_mode(&mut self, mode: OutputMode) {
        self.mode = mode;
    }

    /// Sets the `.headers` state (see [`Repl::set_mode`]).
    pub fn set_headers(&mut self, headers: bool) {
        self.headers = headers;
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

        // Lines are kept verbatim and `\n`-joined: a string literal spanning
        // lines keeps its newline, and parser error positions stay true (#10).
        if !self.buffer.is_empty() {
            self.buffer.push('\n');
        }
        self.buffer.push_str(line);

        if !self.handler.is_complete(&self.buffer) {
            return Step::Continue;
        }
        let buffer = std::mem::take(&mut self.buffer);
        let mut steps: Vec<Step> = self
            .handler
            .statements(&buffer)
            .iter()
            .map(|stmt| self.run_statement(stmt))
            .collect();
        match steps.len() {
            0 => Step::Continue,
            1 => steps.pop().unwrap_or(Step::Continue),
            _ => Step::Many(steps),
        }
    }

    fn run_statement(&mut self, stmt: &str) -> Step {
        match self.handler.execute(stmt) {
            Ok(output) => Step::Print(self.handler.format(&output, self.mode, self.headers)),
            Err(e) => Step::Error(self.handler.error_line(&e)),
        }
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
                    Step::Continue
                }
                None => Step::Error("usage: .mode table|list|column|line|csv|json".to_string()),
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
                _ => Step::Error("usage: .headers on|off".to_string()),
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
                _ => Step::Error("usage: .color on|off".to_string()),
            };
        }

        match self.handler.command(cmd, arg) {
            // A handler that printed on its own (or had nothing to say)
            // returns an empty list: handled, nothing more to emit.
            Some(lines) if lines.is_empty() => Step::Continue,
            Some(lines) => Step::Print(lines.join("\n")),
            None => Step::Error(format!("unknown command: .{cmd}")),
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
    run_repl_with(Repl::new(handler), opts)
}

/// Like [`run_repl`], but over a caller-built [`Repl`] — the way to start
/// in a mode other than `Table`, or with `.headers on` (#10).
pub fn run_repl_with<H: ReplHandler>(repl: Repl<H>, opts: ReplOptions) -> io::Result<()> {
    run_repl_with_editor(repl, Readline::new(), opts)
}

/// Like [`run_repl_with`], but also over a caller-built [`Readline`] — the
/// way to plug in a [`crate::Completer`]/[`crate::Highlighter`]
/// (`editor.set_completer(..)`, `editor.set_highlighter(..)`) before the
/// loop starts owning it (#10).
pub fn run_repl_with_editor<H: ReplHandler>(
    mut repl: Repl<H>,
    mut editor: Readline,
    opts: ReplOptions,
) -> io::Result<()> {
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

        if emit(repl.feed_line(&line)) {
            break;
        }

        editor.set_color(repl.color());
    }

    if let Some(hf) = opts.history_file {
        editor.save_history(hf);
    }
    Ok(())
}

/// Applies one [`Step`] to stdout/stderr; returns `true` on [`Step::Quit`].
fn emit(step: Step) -> bool {
    match step {
        Step::Continue => false,
        // An empty result set renders as "" — print nothing rather than a
        // stray blank line (`sqlite3` prints nothing for zero rows).
        Step::Print(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
            false
        }
        Step::Error(text) => {
            eprintln!("{text}");
            false
        }
        Step::Many(steps) => steps.into_iter().any(emit),
        Step::Quit => true,
    }
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
        assert_eq!(repl.into_handler().executed, vec!["select\n1".to_string()]);
    }

    #[test]
    fn execute_error_is_printed_not_fatal() {
        let mut repl = mock();
        match repl.feed_line("fail;") {
            Step::Error(text) => assert_eq!(text, "error: boom"),
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn quit_and_exit_dot_commands_stop_the_repl() {
        assert!(matches!(mock().feed_line(".quit"), Step::Quit));
        assert!(matches!(mock().feed_line(".exit"), Step::Quit));
    }

    #[test]
    fn mode_command_switches_output_mode_silently() {
        let mut repl = mock();
        assert!(matches!(repl.feed_line(".mode json"), Step::Continue));
        assert_eq!(repl.mode(), OutputMode::Json);
    }

    /// A `sqlite3`-style handler: tokenizer-ish completion (here: a `;`
    /// inside single quotes doesn't count), multi-statement split, and
    /// the `Error:` prefix.
    struct SplitHandler;

    impl ReplHandler for SplitHandler {
        type Output = String;

        fn execute(&mut self, input: &str) -> Result<Self::Output, String> {
            if input == "bad" {
                return Err("boom".to_string());
            }
            Ok(input.to_string())
        }

        fn format(&self, output: &Self::Output, _mode: OutputMode, _headers: bool) -> String {
            output.clone()
        }

        fn is_complete(&self, buffer: &str) -> bool {
            let outside_quotes: String = buffer.split('\'').step_by(2).collect();
            outside_quotes.trim_end().ends_with(';')
        }

        fn statements(&self, buffer: &str) -> Vec<String> {
            // Split on `;` outside single quotes.
            let mut out = Vec::new();
            let mut cur = String::new();
            let mut quoted = false;
            for c in buffer.chars() {
                match c {
                    '\'' => {
                        quoted = !quoted;
                        cur.push(c);
                    }
                    ';' if !quoted => out.push(std::mem::take(&mut cur)),
                    _ => cur.push(c),
                }
            }
            out.push(cur);
            out.into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }

        fn error_line(&self, message: &str) -> String {
            format!("Error: {message}")
        }
    }

    #[test]
    fn is_complete_hook_keeps_buffering_past_a_quoted_semicolon() {
        let mut repl = Repl::new(SplitHandler);
        assert!(matches!(repl.feed_line("select 'a;"), Step::Continue));
        assert!(repl.is_buffering());
        match repl.feed_line("b';") {
            Step::Print(text) => assert_eq!(text, "select 'a;\nb'"),
            _ => panic!("expected Print"),
        }
    }

    #[test]
    fn statements_hook_yields_many_steps_with_errors_routed_to_stderr() {
        let mut repl = Repl::new(SplitHandler);
        match repl.feed_line("one; bad; two;") {
            Step::Many(steps) => {
                assert_eq!(steps.len(), 3);
                assert!(matches!(&steps[0], Step::Print(t) if t == "one"));
                assert!(matches!(&steps[1], Step::Error(t) if t == "Error: boom"));
                assert!(matches!(&steps[2], Step::Print(t) if t == "two"));
            }
            _ => panic!("expected Many"),
        }
    }

    #[test]
    fn handler_command_with_no_lines_is_handled_silently() {
        struct Quiet;
        impl ReplHandler for Quiet {
            type Output = ();
            fn execute(&mut self, _: &str) -> Result<(), String> {
                Ok(())
            }
            fn format(&self, _: &(), _: OutputMode, _: bool) -> String {
                String::new()
            }
            fn command(&mut self, name: &str, _: &str) -> Option<Vec<String>> {
                (name == "quiet").then(Vec::new)
            }
        }
        assert!(matches!(
            Repl::new(Quiet).feed_line(".quiet"),
            Step::Continue
        ));
    }

    #[test]
    fn set_mode_and_headers_change_the_start_state() {
        let mut repl = mock();
        repl.set_mode(OutputMode::List);
        repl.set_headers(true);
        assert_eq!(repl.mode(), OutputMode::List);
        assert!(repl.headers());
    }

    #[test]
    fn unknown_dot_command_reports_itself() {
        match mock().feed_line(".bogus") {
            Step::Error(text) => assert_eq!(text, "unknown command: .bogus"),
            _ => panic!("expected Error"),
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
            Step::Error(text) => assert_eq!(text, "usage: .headers on|off"),
            _ => panic!("expected Error"),
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
            Step::Error(text) => assert_eq!(text, "usage: .color on|off"),
            _ => panic!("expected Error"),
        }
    }
}
