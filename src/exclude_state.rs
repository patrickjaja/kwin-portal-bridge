use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct StoredExcludeState {
    #[serde(default)]
    window_ids: Vec<String>,
}

pub struct ExcludeStateStore {
    path: PathBuf,
}

impl ExcludeStateStore {
    pub fn new() -> Result<Self> {
        let base = crate::util::state_base()?;

        let dir = base.join("kwin-portal-bridge");
        fs::create_dir_all(&dir).context("failed to create exclude-state directory")?;

        Ok(Self {
            path: dir.join("exclude-state.json"),
        })
    }

    pub fn load(&self) -> Result<Vec<String>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let contents =
            fs::read_to_string(&self.path).context("failed to read exclude-state file")?;
        let stored: StoredExcludeState =
            serde_json::from_str(&contents).context("failed to parse exclude-state file")?;
        Ok(stored.window_ids)
    }

    pub fn save(&self, window_ids: &[String]) -> Result<()> {
        let payload = serde_json::to_string_pretty(&StoredExcludeState {
            window_ids: window_ids.to_vec(),
        })?;
        fs::write(&self.path, payload).context("failed to write exclude-state file")
    }

    pub fn clear(&self) -> Result<()> {
        if self.path.exists() {
            fs::remove_file(&self.path).context("failed to remove exclude-state file")?;
        }
        Ok(())
    }
}
