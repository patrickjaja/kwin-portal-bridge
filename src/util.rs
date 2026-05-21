use std::sync::OnceLock;

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
