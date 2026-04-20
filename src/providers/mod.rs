pub mod claude;
pub mod codex;
pub mod qwen;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Qwen,
}

impl Agent {
    pub fn short(self) -> &'static str {
        match self {
            Agent::Claude => "CLA",
            Agent::Codex => "COD",
            Agent::Qwen => "QWN",
        }
    }
}
