use crossterm::event::{Event, KeyCode};
use crossterm::terminal::{disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};

use crossterm::{event, execute};

use std::io;

use crossterm::terminal::enable_raw_mode;

use std::io::Stdout;

use tui::backend::CrosstermBackend;

use tui::Terminal;

use crate::INPUT_POLL;

pub struct TerminalState {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalState {
    pub fn init() -> Result<TerminalState, Box<dyn std::error::Error>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        Ok(TerminalState { terminal })
    }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        disable_raw_mode().expect("disable raw mode");
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen).expect("cleanup");
    }
}

pub fn should_quit() -> Result<bool, Box<dyn std::error::Error>> {
    if event::poll(INPUT_POLL)? {
        if let Event::Key(key) = event::read()? {
            if let KeyCode::Char('q') = key.code {
                return Ok(true);
            }
        }
    }

    Ok(false)
}
