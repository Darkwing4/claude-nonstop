use super::{Key, Multiplexer, PaneAddress};
use crate::error::{Error, Result};
use crate::platform::process;

pub struct Headless;

impl Multiplexer for Headless {
    fn pane_alive(&self, pane: &PaneAddress) -> Result<bool> {
        match pane {
            PaneAddress::Headless { pid } => Ok(process::alive(*pid)),
            other => Err(Error::Invalid(format!(
                "{other} has no driver in this build"
            ))),
        }
    }

    fn foreground_command(&self, _pane: &PaneAddress) -> Result<Option<String>> {
        Ok(None)
    }

    fn capture(&self, pane: &PaneAddress, _full: bool) -> Result<String> {
        Err(Error::Invalid(format!("{pane} has no screen to read")))
    }

    fn send_text(&self, pane: &PaneAddress, _text: &str) -> Result<()> {
        Err(Error::Invalid(format!("{pane} cannot be typed into")))
    }

    fn send_key(&self, pane: &PaneAddress, _key: Key) -> Result<()> {
        Err(Error::Invalid(format!("{pane} cannot be typed into")))
    }
}
