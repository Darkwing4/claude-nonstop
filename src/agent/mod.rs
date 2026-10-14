pub use crate::ledger::AgentId;

pub mod claude;

pub trait Agent {
    fn binary_name(&self) -> &'static str;
    fn is_foreground_command(&self, command: &str) -> bool;
    fn prompt_glyph(&self) -> char;
}

pub fn by_id(id: AgentId) -> Box<dyn Agent> {
    match id {
        AgentId::Claude => Box::new(claude::Claude),
    }
}

impl AgentId {
    pub fn name(self) -> &'static str {
        match self {
            AgentId::Claude => "claude",
        }
    }
}
