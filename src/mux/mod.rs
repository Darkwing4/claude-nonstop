use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;

pub mod headless;
pub mod tmux;
pub mod zellij;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(tag = "mux", rename_all = "snake_case")]
pub enum PaneAddress {
    Tmux { pane_id: String },
    Zellij { session: String, pane_id: u32 },
    Headless { pid: u32 },
}

impl PaneAddress {
    pub fn from_env(env: &BTreeMap<String, String>) -> Option<PaneAddress> {
        if let (Some(session), Some(pane)) =
            (env.get("ZELLIJ_SESSION_NAME"), env.get("ZELLIJ_PANE_ID"))
        {
            let pane_id = pane.parse::<u32>().ok()?;
            return Some(PaneAddress::Zellij {
                session: session.clone(),
                pane_id,
            });
        }
        if let Some(pane) = env.get("TMUX_PANE") {
            if !pane.is_empty() {
                return Some(PaneAddress::Tmux {
                    pane_id: pane.clone(),
                });
            }
        }
        None
    }

    pub fn label(&self) -> String {
        match self {
            PaneAddress::Tmux { pane_id } => format!("tmux {pane_id}"),
            PaneAddress::Zellij { session, pane_id } => format!("zellij {session}#{pane_id}"),
            PaneAddress::Headless { pid } => format!("headless pid {pid}"),
        }
    }

    pub fn is_headless(&self) -> bool {
        matches!(self, PaneAddress::Headless { .. })
    }
}

impl std::fmt::Display for PaneAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Up,
    Down,
}

pub trait Multiplexer {
    fn pane_alive(&self, pane: &PaneAddress) -> Result<bool>;
    fn foreground_command(&self, pane: &PaneAddress) -> Result<Option<String>>;
    fn capture(&self, pane: &PaneAddress, full: bool) -> Result<String>;
    fn send_text(&self, pane: &PaneAddress, text: &str) -> Result<()>;
    fn send_key(&self, pane: &PaneAddress, key: Key) -> Result<()>;
    fn open_window(
        &self,
        session: &str,
        _cwd: &std::path::Path,
        _argv: &[std::ffi::OsString],
    ) -> Result<PaneAddress> {
        Err(crate::error::Error::Invalid(format!(
            "cannot open a window in {session} with this driver"
        )))
    }
}

pub fn for_pane(pane: &PaneAddress) -> Box<dyn Multiplexer> {
    match pane {
        PaneAddress::Zellij { .. } => Box::new(zellij::Zellij),
        PaneAddress::Tmux { .. } => Box::new(tmux::Tmux),
        PaneAddress::Headless { .. } => Box::new(headless::Headless),
    }
}

pub fn env_map() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_address_comes_from_zellij_or_tmux_env() {
        let mut env = BTreeMap::new();
        assert_eq!(PaneAddress::from_env(&env), None);
        env.insert("TMUX_PANE".to_owned(), "%3".to_owned());
        assert_eq!(
            PaneAddress::from_env(&env),
            Some(PaneAddress::Tmux {
                pane_id: "%3".to_owned()
            })
        );
        env.insert("ZELLIJ_SESSION_NAME".to_owned(), "work".to_owned());
        env.insert("ZELLIJ_PANE_ID".to_owned(), "7".to_owned());
        assert_eq!(
            PaneAddress::from_env(&env),
            Some(PaneAddress::Zellij {
                session: "work".to_owned(),
                pane_id: 7
            })
        );
        env.insert("ZELLIJ_PANE_ID".to_owned(), "seven".to_owned());
        assert_eq!(PaneAddress::from_env(&env), None);
    }

    #[test]
    fn pane_address_round_trips_through_json() {
        let pane = PaneAddress::Zellij {
            session: "work".to_owned(),
            pane_id: 7,
        };
        let json = serde_json::to_string(&pane).unwrap();
        assert_eq!(json, r#"{"mux":"zellij","session":"work","pane_id":7}"#);
        assert_eq!(serde_json::from_str::<PaneAddress>(&json).unwrap(), pane);
    }
}
