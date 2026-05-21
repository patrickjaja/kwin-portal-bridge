use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use freedesktop_desktop_entry::{DesktopEntry, Iter, get_languages_from_env};
use freedesktop_icons::lookup;

use crate::model::{InstalledDesktopApp, OpenAppResult};

pub struct DesktopAppService {
    cached: OnceLock<Arc<AliasIndex>>,
}

impl DesktopAppService {
    pub fn new() -> Self {
        Self {
            cached: OnceLock::new(),
        }
    }

    pub fn alias_index(&self) -> Result<Arc<AliasIndex>> {
        if let Some(idx) = self.cached.get() {
            return Ok(idx.clone());
        }
        let idx = Arc::new(build_alias_index()?);
        Ok(self.cached.get_or_init(|| idx).clone())
    }

    pub fn list_installed_apps(&self) -> Result<Vec<InstalledDesktopApp>> {
        Ok(self
            .alias_index()?
            .entries
            .iter()
            .map(|entry| entry.installed_app.clone())
            .collect())
    }

    pub fn get_app_icon(&self, target: &str) -> Result<Option<String>> {
        let index = self.alias_index()?;
        let entry = index.resolve_entry(target)?;
        resolve_icon_data_url(&entry.entry)
    }

    pub fn open_app(&self, target: &str) -> Result<OpenAppResult> {
        let index = self.alias_index()?;
        let entry = index.resolve_entry(target)?;

        let launcher = if entry.entry.dbus_activatable() || entry.entry.exec().is_none() {
            launch_via_kio(&entry.entry)?;
            "kio-launch".to_owned()
        } else {
            match launch_via_exec(&entry.entry) {
                Ok(()) => "desktop-entry-exec".to_owned(),
                Err(exec_error) => {
                    if command_exists("kioclient") {
                        launch_via_kio(&entry.entry).with_context(|| {
                            format!(
                                "desktop entry exec failed ({exec_error:#}), and kio launch fallback also failed"
                            )
                        })?;
                        "kio-launch-fallback".to_owned()
                    } else {
                        return Err(exec_error);
                    }
                }
            }
        };

        Ok(OpenAppResult {
            opened: true,
            bundle_id: entry.installed_app.bundle_id.clone(),
            display_name: entry.installed_app.display_name.clone(),
            path: entry.installed_app.path.clone(),
            launcher,
        })
    }
}

#[derive(Debug, Clone)]
pub struct IndexedDesktopEntry {
    pub installed_app: InstalledDesktopApp,
    pub entry: DesktopEntry,
}

/// Maps every alias a `.desktop` entry exposes (lowercased) to its canonical
/// bundle id. Built once and shared across commands so window-derived raw
/// strings (KWin's `desktop_file_name`, X11 `resource_class`, etc.) and
/// installed-app entries resolve to the same string.
#[derive(Debug, Clone, Default)]
pub struct AliasIndex {
    entries: Vec<IndexedDesktopEntry>,
    aliases: HashMap<String, String>,
}

impl AliasIndex {
    /// Map a raw identifier to its canonical bundle id. Used for
    /// window-derived strings only — bridge inputs are compared verbatim.
    /// Unknown inputs return their normalized self (lowercased, trimmed,
    /// `.desktop`-stripped) as a deterministic fallback.
    pub fn canonicalize(&self, raw: &str) -> String {
        let normalized = normalize_alias_key(raw);
        match self.aliases.get(&normalized) {
            Some(canonical) => canonical.clone(),
            None => normalized,
        }
    }

    /// Test-only builder so other modules can construct a populated index
    /// without walking XDG dirs. Each input pair is `(raw_alias,
    /// canonical_bundle_id)`; the alias is normalized via `normalize_alias_key`
    /// so callers can pass whatever case/suffix variant they like.
    #[cfg(test)]
    pub(crate) fn for_tests<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        let mut aliases = HashMap::new();
        for (alias, canonical) in pairs {
            aliases.insert(normalize_alias_key(alias.as_ref()), canonical.into());
        }
        AliasIndex {
            entries: Vec::new(),
            aliases,
        }
    }

    /// Strict lookup: path equality first, then exact canonical bundle id
    /// match. Upstream owns "did you mean?" — non-canonical inputs error.
    pub fn resolve_entry(&self, target: &str) -> Result<&IndexedDesktopEntry> {
        let target = target.trim();
        if target.is_empty() {
            bail!("app target is empty");
        }

        let target_path = PathBuf::from(target);
        if let Some(found) = self
            .entries
            .iter()
            .find(|entry| entry.entry.path == target_path)
        {
            return Ok(found);
        }

        if let Some(found) = self
            .entries
            .iter()
            .find(|entry| entry.installed_app.bundle_id == target)
        {
            return Ok(found);
        }

        bail!("could not resolve `{target}` to a launchable desktop application")
    }
}

fn normalize_alias_key(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .strip_suffix(".desktop")
        .unwrap_or(trimmed)
        .to_ascii_lowercase()
}

fn build_alias_index() -> Result<AliasIndex> {
    let locales = get_languages_from_env();
    let desktops = current_desktops();
    let mut seen_bundle_ids = HashSet::new();
    let mut entries = Vec::new();
    let mut aliases: HashMap<String, String> = HashMap::new();

    for entry in Iter::new(desktop_entry_dirs().into_iter()).entries(Some(&locales)) {
        if !entry_is_visible(&entry, &desktops) {
            continue;
        }

        let bundle_id = canonical_bundle_id(&entry);
        if bundle_id.is_empty() || !seen_bundle_ids.insert(bundle_id.clone()) {
            continue;
        }

        let display_name = localized_display_name(&entry, &locales)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| bundle_id.clone());

        for alias in collect_aliases(&entry) {
            // Aliases registered for the canonical bundle id win on the first
            // entry that claims them; later entries can't shadow earlier ones.
            aliases.entry(alias).or_insert_with(|| bundle_id.clone());
        }

        entries.push(IndexedDesktopEntry {
            installed_app: InstalledDesktopApp {
                bundle_id,
                display_name,
                path: entry.path.display().to_string(),
            },
            entry,
        });
    }

    Ok(AliasIndex { entries, aliases })
}

fn collect_aliases(entry: &DesktopEntry) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |value: Option<&str>| {
        if let Some(value) = value {
            let normalized = normalize_alias_key(value);
            if !normalized.is_empty() {
                out.push(normalized);
            }
        }
    };

    push(entry.path.file_stem().and_then(OsStr::to_str));
    push(Some(entry.id()));
    push(entry.startup_wm_class());
    if let Some(name) = exec_program_name(entry) {
        push(Some(name.as_str()));
    }
    out
}

fn entry_is_visible(entry: &DesktopEntry, current_desktops: &HashSet<String>) -> bool {
    if entry.type_() != Some("Application") {
        return false;
    }

    if entry.hidden() || entry.no_display() {
        return false;
    }

    if let Some(try_exec) = entry.try_exec()
        && !try_exec_available(try_exec)
    {
        return false;
    }

    if current_desktops.is_empty() {
        return entry.exec().is_some() || entry.dbus_activatable();
    }

    if let Some(only_show_in) = entry.only_show_in()
        && !only_show_in
            .iter()
            .any(|desktop| current_desktops.contains(&desktop.to_ascii_lowercase()))
    {
        return false;
    }

    if let Some(not_show_in) = entry.not_show_in()
        && not_show_in
            .iter()
            .any(|desktop| current_desktops.contains(&desktop.to_ascii_lowercase()))
    {
        return false;
    }

    entry.exec().is_some() || entry.dbus_activatable()
}

fn localized_display_name(entry: &DesktopEntry, locales: &[String]) -> Option<String> {
    entry
        .full_name(locales)
        .or_else(|| entry.name(locales))
        .map(cow_into_owned)
}

fn cow_into_owned(value: Cow<'_, str>) -> String {
    match value {
        Cow::Borrowed(text) => text.to_owned(),
        Cow::Owned(text) => text,
    }
}

fn current_desktops() -> HashSet<String> {
    env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .map(|value| {
            value
                .split(':')
                .map(str::trim)
                .filter(|desktop| !desktop.is_empty())
                .map(|desktop| desktop.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn desktop_entry_dirs() -> Vec<PathBuf> {
    let mut dirs = freedesktop_desktop_entry::default_paths().collect::<Vec<_>>();

    if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/flatpak/exports/share/applications"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));

    let mut seen = HashSet::new();
    dirs.retain(|path| seen.insert(path.clone()));
    dirs
}

fn canonical_bundle_id(entry: &DesktopEntry) -> String {
    // Prefer StartupWMClass: it's what the .desktop file declares its X11
    // windows will report, so windows and installed-app entries agree on the
    // same string without translation. Lowercase to match KWin's
    // case-folded resource_class.
    if let Some(value) = entry
        .startup_wm_class()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return value.to_ascii_lowercase();
    }

    let fallback_id = entry.id();
    let stem = entry
        .path
        .file_stem()
        .and_then(OsStr::to_str)
        .or_else(|| Path::new(fallback_id).file_stem().and_then(OsStr::to_str))
        .unwrap_or(fallback_id);

    normalize_alias_key(stem)
}

fn try_exec_available(try_exec: &str) -> bool {
    let try_exec = try_exec.trim();
    if try_exec.is_empty() {
        return false;
    }

    if try_exec.contains('/') {
        return Path::new(try_exec).exists();
    }

    command_exists(try_exec)
}

fn resolve_icon_data_url(entry: &DesktopEntry) -> Result<Option<String>> {
    let Some(icon_value) = entry
        .icon()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    let icon_path = if Path::new(icon_value).is_absolute() {
        Some(PathBuf::from(icon_value))
    } else {
        let theme = configured_icon_theme();
        lookup(icon_value)
            .with_cache()
            .with_theme(theme.as_str())
            .force_svg()
            .find()
            .or_else(|| {
                if theme == "breeze" {
                    None
                } else {
                    lookup(icon_value)
                        .with_cache()
                        .with_theme("breeze")
                        .force_svg()
                        .find()
                }
            })
    };

    let Some(icon_path) = icon_path else {
        return Ok(None);
    };

    let bytes = fs::read(&icon_path)
        .with_context(|| format!("failed to read icon file `{}`", icon_path.display()))?;
    let mime_type = detect_mime_type(&icon_path);
    let encoded = BASE64_STANDARD.encode(bytes);
    Ok(Some(format!("data:{mime_type};base64,{encoded}")))
}

fn configured_icon_theme() -> String {
    if !command_exists("kreadconfig6") {
        return "breeze".to_owned();
    }

    let output = Command::new("kreadconfig6")
        .args(["--group", "Icons", "--key", "Theme"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let theme = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if theme.is_empty() {
                "breeze".to_owned()
            } else {
                theme
            }
        }
        _ => "breeze".to_owned(),
    }
}

fn detect_mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(OsStr::to_str)
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("svg") | Some("svgz") => "image/svg+xml",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        Some("xpm") => "image/x-xpixmap",
        _ => "application/octet-stream",
    }
}

fn launch_via_kio(entry: &DesktopEntry) -> Result<()> {
    if !command_exists("kioclient") {
        bail!("`kioclient` is not available for desktop-entry launching");
    }

    let mut command = Command::new("kioclient");
    command.arg("exec").arg(&entry.path);
    spawn_detached(&mut command)
}

fn launch_via_exec(entry: &DesktopEntry) -> Result<()> {
    let expanded = expand_exec(entry)?;
    let mut command = Command::new(&expanded.program);
    command.args(&expanded.args);

    if let Some(cwd) = expanded.cwd {
        command.current_dir(cwd);
    }

    if expanded.terminal {
        let (program, args) = wrap_in_terminal(expanded.program, expanded.args);
        let mut terminal_command = Command::new(program);
        terminal_command.args(args);
        return spawn_detached(&mut terminal_command);
    }

    spawn_detached(&mut command)
}

struct ExpandedExec {
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    terminal: bool,
}

fn expand_exec(entry: &DesktopEntry) -> Result<ExpandedExec> {
    let exec = entry
        .exec()
        .map(str::trim)
        .filter(|exec| !exec.is_empty())
        .ok_or_else(|| anyhow::anyhow!("desktop entry has no Exec field"))?;

    let tokens = split_exec(exec)?;
    if tokens.is_empty() {
        bail!("desktop entry Exec field is empty after tokenization");
    }

    let locales = get_languages_from_env();
    let app_name = entry
        .full_name(&locales)
        .or_else(|| entry.name(&locales))
        .map(cow_into_owned);
    let desktop_file_path = entry.path.display().to_string();
    let icon = entry.icon().map(ToOwned::to_owned);

    let mut expanded = Vec::new();
    for token in tokens {
        if token == "%i" {
            if let Some(icon) = icon.clone() {
                expanded.push("--icon".to_owned());
                expanded.push(icon);
            }
            continue;
        }

        let token = expand_exec_token(
            &token,
            app_name.as_deref(),
            icon.as_deref(),
            &desktop_file_path,
        );
        if !token.is_empty() {
            expanded.push(token);
        }
    }

    let mut iter = expanded.into_iter();
    let program = iter
        .next()
        .ok_or_else(|| anyhow::anyhow!("desktop entry Exec field did not expand to a program"))?;

    Ok(ExpandedExec {
        program,
        args: iter.collect(),
        cwd: entry.path().map(PathBuf::from),
        terminal: entry.terminal(),
    })
}

fn expand_exec_token(
    token: &str,
    app_name: Option<&str>,
    icon: Option<&str>,
    desktop_file_path: &str,
) -> String {
    let mut expanded = String::new();
    let mut chars = token.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '%' {
            expanded.push(ch);
            continue;
        }

        let Some(code) = chars.next() else {
            break;
        };

        match code {
            '%' => expanded.push('%'),
            'c' => {
                if let Some(app_name) = app_name {
                    expanded.push_str(app_name);
                }
            }
            'i' => {
                if let Some(icon) = icon {
                    expanded.push_str(icon);
                }
            }
            'k' => expanded.push_str(desktop_file_path),
            'f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'm' | 'v' => {}
            other => {
                expanded.push('%');
                expanded.push(other);
            }
        }
    }

    expanded
}

fn split_exec(exec: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars = exec.chars();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;

    for ch in chars {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }

        match ch {
            '\\' if !in_single => escape = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ch if ch.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if escape || in_single || in_double {
        bail!("unterminated quoting in desktop entry Exec field");
    }

    if !current.is_empty() {
        parts.push(current);
    }

    Ok(parts)
}

fn exec_program_name(entry: &DesktopEntry) -> Option<String> {
    let exec = entry.exec()?.trim();
    let tokens = split_exec(exec).ok()?;
    let program = tokens.first()?;
    Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .map(ToOwned::to_owned)
}

fn command_exists(command: &str) -> bool {
    if command.contains('/') {
        return Path::new(command).exists();
    }

    let Some(paths) = env::var_os("PATH") else {
        return false;
    };

    env::split_paths(&paths).any(|dir| dir.join(command).exists())
}

fn wrap_in_terminal(program: String, args: Vec<String>) -> (String, Vec<String>) {
    for (terminal, prefix) in [
        ("x-terminal-emulator", vec!["-e"]),
        ("konsole", vec!["-e"]),
        ("gnome-terminal", vec!["--"]),
        ("alacritty", vec!["-e"]),
        ("kitty", Vec::<&str>::new()),
        ("xterm", vec!["-e"]),
    ] {
        if command_exists(terminal) {
            let mut wrapped_args = prefix
                .into_iter()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            wrapped_args.push(program);
            wrapped_args.extend(args);
            return (terminal.to_owned(), wrapped_args);
        }
    }

    (program, args)
}

fn spawn_detached(command: &mut Command) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .context("failed to spawn detached desktop entry command")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{expand_exec_token, split_exec};

    #[test]
    fn split_exec_handles_quotes_and_escapes() {
        let parsed = split_exec(r#"app --flag "hello world" 'two words'"#).unwrap();
        assert_eq!(parsed, vec!["app", "--flag", "hello world", "two words"]);
    }

    #[test]
    fn expand_exec_token_replaces_basic_field_codes() {
        let expanded = expand_exec_token(
            "BAMF_DESKTOP_FILE_HINT=%k",
            Some("Firefox"),
            Some("firefox"),
            "/usr/share/applications/firefox.desktop",
        );
        assert_eq!(
            expanded,
            "BAMF_DESKTOP_FILE_HINT=/usr/share/applications/firefox.desktop"
        );

        let name = expand_exec_token("%c", Some("Firefox"), Some("firefox"), "/tmp/app.desktop");
        assert_eq!(name, "Firefox");
    }
}
