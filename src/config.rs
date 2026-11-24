//! An experiment directory: `config.json`, `src/task.rs` and `src/dependencies.rs`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub name: String,
    pub description: String,
    /// Dependency symbols shown in full. Methods are written `Type::method`.
    #[serde(default)]
    pub public: Vec<String>,
    /// Dependency functions shown without their bodies. Anything unlisted is hidden.
    #[serde(default)]
    pub interface_only: Vec<String>,
    #[serde(default)]
    pub web_search: bool,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub tool_timeout_secs: u64,
    pub verus_timeout_secs: u64,
    pub experiment_timeout_secs: u64,
    pub max_cost_usd: f64,
    pub max_proposals: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            tool_timeout_secs: 60,
            verus_timeout_secs: 120,
            experiment_timeout_secs: 1800,
            max_cost_usd: 2.0,
            max_proposals: 10,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Experiment {
    pub config: Config,
    pub task: String,
    pub dependencies: String,
}

impl Experiment {
    pub fn load(dir: &Path) -> Result<Self> {
        let read = |p: &str| {
            let path = dir.join(p);
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
        };
        Ok(Experiment {
            config: serde_json::from_str(&read("config.json")?).context("parsing config.json")?,
            task: read("src/task.rs")?,
            dependencies: read("src/dependencies.rs")?,
        })
    }
}
