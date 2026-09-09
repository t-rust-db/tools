//! The app loop wiring the query pane in (db-studio#3). Grid/error panes
//! (#4/#5) replace `last_submitted`'s placeholder rendering once they land.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::query_pane::QueryPane;
use crate::terminal::Tui;

pub struct App {
    running: bool,
    query_pane: QueryPane,
    last_submitted: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            running: true,
            query_pane: QueryPane::new(),
            last_submitted: None,
        }
    }

    pub fn run(&mut self, terminal: &mut Tui) -> io::Result<()> {
        while self.running {
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn draw(&self, frame: &mut Frame) {
        let [query_area, output_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(frame.area());
        self.query_pane.render(frame, query_area);
        let placeholder = self
            .last_submitted
            .as_deref()
            .map(|q| format!("submitted: {q}"))
            .unwrap_or_default();
        frame.render_widget(Paragraph::new(placeholder), output_area);
    }

    fn handle_events(&mut self) -> io::Result<()> {
        // Bounded poll so the loop can react to a resize/redraw even with
        // no key pressed, without spinning the CPU.
        if !event::poll(Duration::from_millis(250))? {
            return Ok(());
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }
            // `q` is a valid SQL character, so it can no longer double as
            // quit now that the query pane accepts arbitrary text (#1's
            // scaffold had no text input yet, so it was safe there).
            let is_ctrl_c =
                key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
            if is_ctrl_c || key.code == KeyCode::Esc {
                self.running = false;
                return Ok(());
            }
            if let Some(query) = self.query_pane.handle_key(key) {
                self.last_submitted = Some(query);
            }
        }
        Ok(())
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
