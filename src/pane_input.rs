use std::time::Duration;

use crate::agent::Agent;
use crate::error::{Error, Result};
use crate::mux::{Key, Multiplexer, PaneAddress};
use crate::screen;

pub const SUBMIT_GAP: Duration = Duration::from_millis(150);

pub struct Typing<'a> {
    pub mux: &'a dyn Multiplexer,
    pub pane: &'a PaneAddress,
    pub agent: &'a dyn Agent,
}

impl Typing<'_> {
    pub fn ensure_alive(&self) -> Result<()> {
        if self.mux.pane_alive(self.pane)? {
            Ok(())
        } else {
            Err(Error::Invalid(format!("{} is gone", self.pane)))
        }
    }

    pub fn ensure_foreground(&self) -> Result<()> {
        match self.mux.foreground_command(self.pane)? {
            Some(command) if !self.agent.is_foreground_command(&command) => {
                Err(Error::Invalid(format!(
                    "{} runs {command}, not {}",
                    self.pane,
                    self.agent.binary_name()
                )))
            }
            _ => Ok(()),
        }
    }

    pub fn ensure_idle_prompt(&self) -> Result<()> {
        let visible = self.mux.capture(self.pane, false)?;
        let text = screen::strip_ansi(visible.as_bytes());
        if screen::is_busy(&text) {
            return Err(Error::Invalid(format!("{} is busy with a turn", self.pane)));
        }
        match screen::input_line(&text, self.agent.prompt_glyph()) {
            Some(line) if !line.is_empty() => Err(Error::Invalid(format!(
                "{} has text in its input line ({} chars), the user may be typing",
                self.pane,
                line.chars().count()
            ))),
            _ => Ok(()),
        }
    }

    pub fn press(&self, key: Key) -> Result<()> {
        self.ensure_alive()?;
        self.mux.send_key(self.pane, key)
    }

    pub fn submit(&self, text: &str) -> Result<()> {
        self.ensure_alive()?;
        self.ensure_foreground()?;
        self.ensure_idle_prompt()?;
        self.mux.send_text(self.pane, text)?;
        std::thread::sleep(SUBMIT_GAP);
        self.mux.send_key(self.pane, Key::Enter)
    }
}
