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
        let store = Self::locate()?;
        if let Some(dir) = store.path.parent() {
            fs::create_dir_all(dir).context("failed to create token store directory")?;
        }
        Ok(store)
    }

    /// The store at its default path, without touching the filesystem (for
    /// `forget-session`, which must not create state it is about to delete).
    pub fn locate() -> Result<Self> {
        let base = crate::util::state_base()?;

        Ok(Self {
            path: base.join("kwin-portal-bridge").join("restore-token.json"),
        })
    }

    /// Delete the stored token. Returns whether there was one.
    pub fn remove(&self) -> Result<bool> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| {
                format!("failed to delete restore token `{}`", self.path.display())
            }),
        }
    }

    /// Where the token is stored.
    pub fn path(&self) -> &std::path::Path {
        &self.path
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &std::path::Path) -> TokenStore {
        TokenStore {
            path: dir.join("kwin-portal-bridge").join("restore-token.json"),
        }
    }

    #[test]
    fn remove_deletes_the_token_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        store.save("secret").unwrap();
        let sibling = store.path().with_file_name("exclude-state.json");
        std::fs::write(&sibling, "{}").unwrap();

        assert!(store.remove().unwrap());
        assert!(!store.path().exists());
        assert_eq!(store.load().unwrap(), None);
        assert!(sibling.exists(), "other state must be left alone");
        assert!(!store.remove().unwrap(), "second removal finds nothing");
    }

    #[test]
    fn remove_without_a_state_directory_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        assert!(!store.remove().unwrap());
        assert!(!tmp.path().join("kwin-portal-bridge").exists());
    }

    #[test]
    fn remove_reports_a_token_that_cannot_be_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        // A directory where the file should be: remove_file fails with a
        // non-NotFound error, which must surface rather than read as "absent".
        std::fs::create_dir_all(store.path()).unwrap();
        let error = store.remove().unwrap_err();
        assert!(format!("{error:#}").contains("failed to delete restore token"));
    }

    #[test]
    fn locate_honours_xdg_state_home() {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: the only test in this crate that touches XDG_STATE_HOME.
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.path()) };
        let store = TokenStore::locate().unwrap();
        assert_eq!(
            store.path(),
            tmp.path().join("kwin-portal-bridge/restore-token.json")
        );
        assert!(!tmp.path().join("kwin-portal-bridge").exists());
    }
}
