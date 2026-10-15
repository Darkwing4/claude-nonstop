use std::env;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub state: PathBuf,
    pub config_file: PathBuf,
    pub claude_config: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Paths> {
        let home = env_path("HOME").ok_or_else(|| Error::Missing("HOME".to_owned()))?;
        let state = env_path("CLAUDE_NONSTOP_STATE_DIR")
            .unwrap_or_else(|| xdg("XDG_STATE_HOME", &home, ".local/state").join("claude-nonstop"));
        let config_file = env_path("CLAUDE_NONSTOP_CONFIG").unwrap_or_else(|| {
            xdg("XDG_CONFIG_HOME", &home, ".config").join("claude-nonstop/config.toml")
        });
        let claude_config = env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
        Ok(Paths {
            home,
            state,
            config_file,
            claude_config,
        })
    }

    pub fn ledger(&self) -> PathBuf {
        self.state.join("ledger.json")
    }

    pub fn ledger_lock(&self) -> PathBuf {
        self.state.join("ledger.lock")
    }

    pub fn resumer_lock(&self) -> PathBuf {
        self.state.join("resumer.lock")
    }

    pub fn headless_dir(&self) -> PathBuf {
        self.state.join("headless")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.state.join("log")
    }

    pub fn log_file(&self) -> PathBuf {
        self.log_dir().join("claude-nonstop.log")
    }

    pub fn claude_settings(&self) -> PathBuf {
        self.claude_config.join("settings.json")
    }

    pub fn claude_sessions_dir(&self) -> PathBuf {
        self.claude_config.join("sessions")
    }

    pub fn claude_projects_dir(&self) -> PathBuf {
        self.claude_config.join("projects")
    }

    pub fn ensure_state_dirs(&self) -> Result<()> {
        for dir in [self.state.clone(), self.headless_dir(), self.log_dir()] {
            create_private_dir(&dir)?;
        }
        Ok(())
    }
}

pub fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|source| Error::io(dir, "create", source))
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn xdg(name: &str, home: &Path, fallback: &str) -> PathBuf {
    env_path(name).unwrap_or_else(|| home.join(fallback))
}
