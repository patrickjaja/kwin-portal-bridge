use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Result, bail};

/// Shell `${VAR:-...}` semantics, as the contract and the launcher use: an
/// empty variable counts as unset (an empty base would yield relative paths).
fn non_empty(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// `${XDG_STATE_HOME:-$HOME/.local/state}`.
pub fn state_base() -> Result<PathBuf> {
    state_base_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn state_base_from(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(base) = non_empty(xdg_state_home) {
        return Ok(base);
    }
    match non_empty(home) {
        Some(home) => Ok(home.join(".local/state")),
        None => bail!("HOME is not set"),
    }
}

/// `${XDG_RUNTIME_DIR:-/tmp}`.
pub fn runtime_base() -> PathBuf {
    runtime_base_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

fn runtime_base_from(xdg_runtime_dir: Option<OsString>) -> PathBuf {
    non_empty(xdg_runtime_dir).unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Lowercased identifiers that mark a window as belonging to this bridge
/// process — including its teach and session overlays, which inherit the
/// binary's `resourceClass` / `resourceName`. Includes both the cargo
/// package name (compile-time) and the running executable's basename
/// (runtime), since the binary can be renamed at install time.
pub fn bridge_overlay_names() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = vec![env!("CARGO_PKG_NAME").to_ascii_lowercase()];
        if let Ok(exe) = std::env::current_exe()
            && let Some(stem) = exe.file_stem().and_then(|stem| stem.to_str())
        {
            let normalized = stem.trim().to_ascii_lowercase();
            if !normalized.is_empty() && !names.contains(&normalized) {
                names.push(normalized);
            }
        }
        names
    })
}

pub fn rects_intersect(
    ax: i32,
    ay: i32,
    aw: i32,
    ah: i32,
    bx: i32,
    by: i32,
    bw: i32,
    bh: i32,
) -> bool {
    if aw <= 0 || ah <= 0 || bw <= 0 || bh <= 0 {
        return false;
    }

    let a_right = ax.saturating_add(aw);
    let a_bottom = ay.saturating_add(ah);
    let b_right = bx.saturating_add(bw);
    let b_bottom = by.saturating_add(bh);

    ax < b_right && a_right > bx && ay < b_bottom && a_bottom > by
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(value: &str) -> Option<OsString> {
        Some(OsString::from(value))
    }

    #[test]
    fn state_base_treats_empty_as_unset() {
        assert_eq!(
            state_base_from(os("/x/state"), os("/home/u")).unwrap(),
            PathBuf::from("/x/state")
        );
        for unset in [None, os("")] {
            assert_eq!(
                state_base_from(unset, os("/home/u")).unwrap(),
                PathBuf::from("/home/u/.local/state")
            );
        }
        for no_home in [None, os("")] {
            assert!(state_base_from(os(""), no_home).is_err());
        }
    }

    #[test]
    fn runtime_base_treats_empty_as_unset() {
        assert_eq!(
            runtime_base_from(os("/run/user/1000")),
            PathBuf::from("/run/user/1000")
        );
        assert_eq!(runtime_base_from(os("")), PathBuf::from("/tmp"));
        assert_eq!(runtime_base_from(None), PathBuf::from("/tmp"));
    }
}
