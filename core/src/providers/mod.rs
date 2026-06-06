pub mod claude;
pub mod codex;
pub mod hermes;
pub mod omp;
pub mod qwen;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Hermes,
    Omp,
    Qwen,
}

impl Agent {
    pub fn short(self) -> &'static str {
        match self {
            Agent::Claude => "CLA",
            Agent::Codex => "COD",
            Agent::Hermes => "HER",
            Agent::Omp => "OMP",
            Agent::Qwen => "QWN",
        }
    }
}
