use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    token: String,
}

pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME is not set")?;
        let base = std::env::var("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(home).join(".local/state"));

        let dir = base.join("kwin-portal-bridge");
        fs::create_dir_all(&dir).context("failed to create token store directory")?;

        Ok(Self {
            path: dir.join("restore-token.json"),
        })
    }

    pub fn load(&self) -> Result<Option<String>> {
        if !self.path.exists() {
            return Ok(None);
        }

        let contents =
            fs::read_to_string(&self.path).context("failed to read restore token file")?;
        let stored: StoredToken =
            serde_json::from_str(&contents).context("failed to parse restore token file")?;
        Ok(Some(stored.token))
    }

    pub fn save(&self, token: &str) -> Result<()> {
        let payload = serde_json::to_string_pretty(&StoredToken {
            token: token.to_owned(),
        })?;

        // Write-then-rename keeps the token file atomic: a crash or a second
        // concurrent session mid-write would otherwise leave truncated JSON,
        // silently dropping the token and re-prompting for capture consent.
        let dir = self
            .path
            .parent()
            .context("restore token path has no parent directory")?;
        let mut temp = tempfile::NamedTempFile::new_in(dir)
            .context("failed to create restore token temp file")?;
        temp.write_all(payload.as_bytes())
            .context("failed to write restore token file")?;
        temp.persist(&self.path)
            .context("failed to persist restore token file")?;
        Ok(())
    }
}
