use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

const MAX_ANCESTORS: usize = 256;

#[derive(Clone, Debug, Serialize)]
pub struct DesktopApp {
    pub id: String,
    pub name: String,
    pub executable: Option<String>,
    pub hidden: bool,
    pub no_display: bool,
}

#[derive(Debug)]
struct ParsedDesktopEntry {
    name: String,
    executable: Option<String>,
    try_exec: Option<String>,
    hidden: bool,
    no_display: bool,
    _dbus_activatable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessStat {
    ppid: u32,
    session: u32,
}

#[derive(Debug)]
struct DesktopAppNotFound(String);

impl std::fmt::Display for DesktopAppNotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DesktopAppNotFound {}

pub fn is_desktop_app_not_found(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DesktopAppNotFound>().is_some()
}

pub fn installed_apps() -> Result<Vec<DesktopApp>> {
    discover_apps(&xdg_application_dirs())
}

pub fn resolve_desktop_app(query: &str) -> Result<DesktopApp> {
    resolve_from_apps(query, &installed_apps()?)
}

pub async fn launch_app(app: &DesktopApp) -> Result<()> {
    if app.hidden {
        bail!("hidden desktop application {} cannot be launched", app.id);
    }
    enable_accessibility_status().await?;

    let mut command = if app.id == "steam.desktop" {
        let executable = steam_executable(app)?;
        let mut command = Command::new(executable);
        command.arg("-cef-force-accessibility");
        command
    } else {
        let gtk_launch = find_executable("gtk-launch")
            .ok_or_else(|| anyhow!("gtk-launch was not found as an executable in PATH"))?;
        let desktop_id = app
            .id
            .strip_suffix(".desktop")
            .ok_or_else(|| anyhow!("desktop ID {:?} does not end in .desktop", app.id))?;
        let mut command = Command::new(gtk_launch);
        command.arg(desktop_id);
        command
    };

    sanitize_accessibility_environment(&mut command);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command
        .spawn()
        .with_context(|| format!("failed to launch desktop application {}", app.id))?;
    Ok(())
}

pub async fn enable_accessibility_status() -> Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("failed to connect to the session D-Bus for AT-SPI")?;
    let status = zbus::Proxy::new(
        &connection,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Status",
    )
    .await
    .context(
        "AT-SPI status interface org.a11y.Status is unavailable at org.a11y.Bus /org/a11y/bus",
    )?;

    status
        .set_property("ScreenReaderEnabled", true)
        .await
        .context(
            "failed to set org.a11y.Status.ScreenReaderEnabled; the session AT-SPI interface may be unsupported",
        )?;
    status.set_property("IsEnabled", true).await.context(
        "failed to set org.a11y.Status.IsEnabled; the session AT-SPI interface may be unsupported",
    )?;

    let screen_reader_enabled: bool = status
        .get_property("ScreenReaderEnabled")
        .await
        .context("failed to read back org.a11y.Status.ScreenReaderEnabled")?;
    let is_enabled: bool = status
        .get_property("IsEnabled")
        .await
        .context("failed to read back org.a11y.Status.IsEnabled")?;
    if !screen_reader_enabled || !is_enabled {
        bail!(
            "AT-SPI status verification failed: ScreenReaderEnabled={screen_reader_enabled}, IsEnabled={is_enabled}"
        );
    }
    Ok(())
}

pub fn process_family(pid: u32) -> Result<HashSet<u32>> {
    if pid == 0 || pid == 1 {
        bail!("PID {pid} is not a user-session process");
    }

    let target = read_process_stat(pid)?;
    let mut stats = HashMap::new();
    let proc_entries = fs::read_dir("/proc").context("failed to scan /proc")?;
    for entry in proc_entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(candidate_pid) = parse_numeric_pid(&entry.file_name()) else {
            continue;
        };
        if candidate_pid == 0 {
            continue;
        }
        if let Ok(stat) = read_process_stat(candidate_pid) {
            stats.insert(candidate_pid, stat);
        }
    }
    stats.insert(pid, target);

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&child, stat) in &stats {
        children.entry(stat.ppid).or_default().push(child);
    }

    let mut family = HashSet::from([pid]);
    let mut queue = VecDeque::from([pid]);
    while let Some(parent) = queue.pop_front() {
        if let Some(direct_children) = children.get(&parent) {
            for &child in direct_children {
                if child != 1 && family.insert(child) {
                    queue.push_back(child);
                }
            }
        }
    }

    let mut current = pid;
    let mut visited = HashSet::from([pid]);
    for _ in 0..MAX_ANCESTORS {
        let stat = stats
            .get(&current)
            .copied()
            .or_else(|| read_process_stat(current).ok())
            .ok_or_else(|| anyhow!("failed to read ancestry for PID {current}"))?;
        let parent = stat.ppid;
        if parent <= 1 || !visited.insert(parent) {
            break;
        }
        let parent_stat = stats
            .get(&parent)
            .copied()
            .or_else(|| read_process_stat(parent).ok())
            .ok_or_else(|| anyhow!("failed to read parent PID {parent}"))?;
        if parent_stat.session != target.session {
            break;
        }
        family.insert(parent);
        current = parent;
    }

    Ok(family)
}

pub fn processes_related(a: u32, b: u32) -> bool {
    if a == 0 || b == 0 || a == 1 || b == 1 {
        return false;
    }
    if a == b {
        return read_process_stat(a).is_ok();
    }
    is_session_ancestor(a, b) || is_session_ancestor(b, a)
}

pub fn process_is_same_or_descendant(ancestor: u32, candidate: u32) -> bool {
    if ancestor == 0 || candidate == 0 || ancestor == 1 || candidate == 1 {
        return false;
    }
    if ancestor == candidate {
        return read_process_stat(ancestor).is_ok();
    }
    is_session_ancestor(ancestor, candidate)
}

fn xdg_application_dirs() -> Vec<PathBuf> {
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from(".local/share"));

    let data_dirs = env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsStr::new("/usr/local/share:/usr/share").to_os_string());

    let mut roots = vec![data_home.join("applications")];
    roots.extend(
        env::split_paths(&data_dirs)
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.join("applications")),
    );
    roots
}

fn discover_apps(roots: &[PathBuf]) -> Result<Vec<DesktopApp>> {
    let mut seen_ids = HashSet::new();
    let mut apps = Vec::new();

    for root in roots {
        let mut files = Vec::new();
        collect_desktop_files(root, root, &mut files)?;
        files.sort();

        for path in files {
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("{} is not below {}", path.display(), root.display()))?;
            let id = desktop_id(relative);
            if !seen_ids.insert(id.clone()) {
                continue;
            }

            let bytes = fs::read(&path)
                .with_context(|| format!("failed to read desktop entry {}", path.display()))?;
            let Some(entry) = parse_desktop_entry(&String::from_utf8_lossy(&bytes)) else {
                continue;
            };
            if entry.hidden {
                continue;
            }
            if let Some(try_exec) = &entry.try_exec {
                if find_executable(try_exec).is_none() {
                    continue;
                }
            }

            apps.push(DesktopApp {
                id,
                name: entry.name,
                executable: entry.executable,
                hidden: false,
                no_display: entry.no_display,
            });
        }
    }

    apps.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(apps)
}

fn collect_desktop_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if directory == root && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read application directory {}",
                    directory.display()
                )
            });
        }
    };

    for entry in entries {
        let entry = entry
            .with_context(|| format!("failed to read an entry below {}", directory.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if file_type.is_dir() {
            collect_desktop_files(root, &entry.path(), files)?;
        } else if file_type.is_file() && entry.path().extension() == Some(OsStr::new("desktop")) {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn desktop_id(relative_path: &Path) -> String {
    relative_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("-")
}

fn parse_desktop_entry(contents: &str) -> Option<ParsedDesktopEntry> {
    let mut in_desktop_entry = false;
    let mut application_type = None;
    let mut name = None;
    let mut exec = None;
    let mut try_exec = None;
    let mut hidden = false;
    let mut no_display = false;
    let mut dbus_activatable = false;

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Type" => application_type = Some(value.trim().to_owned()),
            "Name" => name = Some(value.trim().to_owned()),
            "Exec" => exec = Some(value.trim().to_owned()),
            "TryExec" => try_exec = Some(value.trim().to_owned()),
            "Hidden" => hidden = parse_desktop_bool(value),
            "NoDisplay" => no_display = parse_desktop_bool(value),
            "DBusActivatable" => dbus_activatable = parse_desktop_bool(value),
            _ => {}
        }
    }

    if application_type.as_deref() != Some("Application") {
        return None;
    }
    let name = name.filter(|name| !name.is_empty())?;
    Some(ParsedDesktopEntry {
        name,
        executable: exec.as_deref().and_then(parse_first_exec_arg),
        try_exec: try_exec.filter(|value| !value.is_empty()),
        hidden,
        no_display,
        _dbus_activatable: dbus_activatable,
    })
}

fn parse_desktop_bool(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("true")
}

fn parse_first_exec_arg(exec: &str) -> Option<String> {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut quote = Quote::None;
    let mut escaped = false;
    let mut started = false;
    let mut argument = String::new();

    for character in exec.trim_start().chars() {
        if escaped {
            argument.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match (quote, character) {
            (Quote::None, '\\') | (Quote::Double, '\\') => {
                escaped = true;
                started = true;
            }
            (Quote::None, '\'') => {
                quote = Quote::Single;
                started = true;
            }
            (Quote::None, '"') => {
                quote = Quote::Double;
                started = true;
            }
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, value) if value.is_whitespace() => {
                if started {
                    break;
                }
            }
            (_, value) => {
                argument.push(value);
                started = true;
            }
        }
    }

    if !started
        || escaped
        || quote != Quote::None
        || argument.is_empty()
        || argument.contains('%')
        || argument.contains('\0')
    {
        None
    } else {
        Some(argument)
    }
}

fn resolve_from_apps(query: &str, apps: &[DesktopApp]) -> Result<DesktopApp> {
    let query = query.trim();
    if query.is_empty() {
        bail!("desktop application query must not be empty");
    }

    let exact_id = if query.ends_with(".desktop") {
        query.to_owned()
    } else {
        format!("{query}.desktop")
    };
    if let Some(app) = apps.iter().find(|app| app.id == exact_id) {
        return Ok(app.clone());
    }

    let folded_query = query.to_lowercase();
    let mut name_matches: Vec<&DesktopApp> = apps
        .iter()
        .filter(|app| app.name.to_lowercase() == folded_query)
        .collect();
    name_matches.sort_by(|left, right| left.id.cmp(&right.id));
    match name_matches.as_slice() {
        [] => Err(
            DesktopAppNotFound(format!("no desktop application exactly matches {query:?}")).into(),
        ),
        [app] => Ok((*app).clone()),
        matches => {
            let ids = matches
                .iter()
                .map(|app| app.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("desktop application name {query:?} is ambiguous; matching IDs: {ids}")
        }
    }
}

fn find_executable(value: &str) -> Option<PathBuf> {
    let path = Path::new(value);
    if path.components().count() > 1 || path.is_absolute() {
        return is_executable_file(path).then(|| path.to_path_buf());
    }
    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(value))
        .find(|candidate| is_executable_file(candidate))
}

fn steam_executable(app: &DesktopApp) -> Result<PathBuf> {
    let executable = match app.executable.as_deref().filter(|value| !value.is_empty()) {
        Some(value) => find_executable(value).with_context(|| {
            format!(
                "Steam desktop Exec executable {value:?} is not an executable file or was not found in PATH"
            )
        })?,
        None => {
            let fallback = PathBuf::from("/usr/bin/steam");
            if !is_executable_file(&fallback) {
                bail!(
                    "steam.desktop has no safely parseable Exec executable and /usr/bin/steam is not executable"
                );
            }
            fallback
        }
    };
    let basename = executable.file_name().and_then(OsStr::to_str);
    if !matches!(basename, Some("steam" | "steam-runtime")) {
        bail!(
            "unsupported Steam desktop Exec executable {:?}; expected direct steam or steam-runtime",
            executable.display()
        );
    }
    Ok(executable)
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn sanitize_accessibility_environment(command: &mut Command) {
    command.env_remove("NO_AT_BRIDGE");
    remove_env_if(command, "GTK_A11Y", |value| {
        value.eq_ignore_ascii_case("none")
    });
    remove_env_if(command, "ACCESSIBILITY_ENABLED", |value| value == "0");
    remove_env_if(command, "GNOME_ACCESSIBILITY", |value| value == "0");
    remove_env_if(command, "QT_ACCESSIBILITY", |value| value == "0");
    command.env("ACCESSIBILITY_ENABLED", "1");
    command.env("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1");
}

fn remove_env_if(command: &mut Command, key: &str, predicate: impl FnOnce(&str) -> bool) {
    if env::var(key).ok().as_deref().is_some_and(predicate) {
        command.env_remove(key);
    }
}

fn parse_numeric_pid(value: &OsStr) -> Option<u32> {
    value.to_str()?.parse().ok()
}

fn read_process_stat(pid: u32) -> Result<ProcessStat> {
    let path = format!("/proc/{pid}/stat");
    let contents = fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;
    parse_process_stat_line(&contents).with_context(|| format!("failed to parse {path}"))
}

fn parse_process_stat_line(line: &str) -> Result<ProcessStat> {
    let close = line
        .rfind(')')
        .ok_or_else(|| anyhow!("process stat has no closing command parenthesis"))?;
    let fields: Vec<&str> = line[close + 1..].split_whitespace().collect();
    if fields.len() < 4 {
        bail!("process stat is missing state, PPid, process group, or session fields");
    }
    let ppid = fields[1]
        .parse()
        .context("process stat PPid is not an unsigned integer")?;
    let session = fields[3]
        .parse()
        .context("process stat session is not an unsigned integer")?;
    Ok(ProcessStat { ppid, session })
}

fn is_session_ancestor(ancestor: u32, descendant: u32) -> bool {
    let Ok(start) = read_process_stat(descendant) else {
        return false;
    };
    let mut current = descendant;
    let mut visited = HashSet::from([descendant]);

    for _ in 0..MAX_ANCESTORS {
        let Ok(stat) = read_process_stat(current) else {
            return false;
        };
        if stat.session != start.session || stat.ppid <= 1 || !visited.insert(stat.ppid) {
            return false;
        }
        let Ok(parent_stat) = read_process_stat(stat.ppid) else {
            return false;
        };
        if parent_stat.session != start.session {
            return false;
        }
        if stat.ppid == ancestor {
            return true;
        }
        current = stat.ppid;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "sky-wayland-apps-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn app(id: &str, name: &str) -> DesktopApp {
        DesktopApp {
            id: id.to_owned(),
            name: name.to_owned(),
            executable: None,
            hidden: false,
            no_display: false,
        }
    }

    #[test]
    fn parses_only_base_desktop_entry_fields() {
        let parsed = parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Editor\nName[de]=Editor DE\n\
             Exec=\"/opt/My Editor/bin/editor\" %U\nHidden=false\nNoDisplay=true\n\
             DBusActivatable=true\n\n[Other]\nName=Wrong\n",
        )
        .unwrap();

        assert_eq!(parsed.name, "Editor");
        assert_eq!(
            parsed.executable.as_deref(),
            Some("/opt/My Editor/bin/editor")
        );
        assert!(!parsed.hidden);
        assert!(parsed.no_display);
        assert!(parsed._dbus_activatable);
    }

    #[test]
    fn precedence_and_hidden_entries_mask_lower_roots() {
        let high = TestDir::new("high");
        let low = TestDir::new("low");
        fs::create_dir(high.0.join("nested")).unwrap();
        fs::write(
            high.0.join("nested-visible.desktop"),
            "[Desktop Entry]\nType=Application\nName=High\nExec=high\n",
        )
        .unwrap();
        fs::write(
            high.0.join("masked.desktop"),
            "[Desktop Entry]\nType=Application\nName=Hidden\nHidden=true\n",
        )
        .unwrap();
        fs::write(
            low.0.join("nested-visible.desktop"),
            "[Desktop Entry]\nType=Application\nName=Low\n",
        )
        .unwrap();
        fs::write(
            low.0.join("masked.desktop"),
            "[Desktop Entry]\nType=Application\nName=Must Not Leak\n",
        )
        .unwrap();
        fs::write(
            high.0.join("nested").join("tool.desktop"),
            "[Desktop Entry]\nType=Application\nName=Nested\n",
        )
        .unwrap();

        let apps = discover_apps(&[high.0.clone(), low.0.clone()]).unwrap();
        assert_eq!(
            apps.iter().map(|app| app.id.as_str()).collect::<Vec<_>>(),
            vec!["nested-tool.desktop", "nested-visible.desktop"]
        );
        assert_eq!(
            apps.iter()
                .find(|app| app.id == "nested-visible.desktop")
                .unwrap()
                .name,
            "High"
        );
    }

    #[test]
    fn exact_resolution_rejects_ambiguity_and_substrings() {
        let apps = vec![
            app("one.desktop", "Editor"),
            app("two.desktop", "EDITOR"),
            app("terminal.desktop", "Terminal"),
        ];

        assert_eq!(
            resolve_from_apps("terminal", &apps).unwrap().id,
            "terminal.desktop"
        );
        let ambiguity = resolve_from_apps("editor", &apps).unwrap_err().to_string();
        assert!(ambiguity.contains("one.desktop"));
        assert!(ambiguity.contains("two.desktop"));
        assert!(resolve_from_apps("Term", &apps).is_err());
        assert!(resolve_from_apps(" ", &apps).is_err());
    }

    #[test]
    fn parses_stat_with_spaces_and_parentheses_in_comm() {
        let stat = parse_process_stat_line("321 (a tricky) process) S 123 456 789 0 0").unwrap();
        assert_eq!(
            stat,
            ProcessStat {
                ppid: 123,
                session: 789
            }
        );
    }

    #[test]
    fn relation_graph_requires_directional_ancestry() {
        let parents = HashMap::from([(20, 10), (30, 10), (40, 20)]);
        assert!(same_or_descendant_in_parent_map(10, 40, &parents));
        assert!(!same_or_descendant_in_parent_map(40, 10, &parents));
        assert!(!same_or_descendant_in_parent_map(20, 30, &parents));
        assert!(same_or_descendant_in_parent_map(20, 20, &parents));
    }

    fn same_or_descendant_in_parent_map(
        ancestor: u32,
        mut candidate: u32,
        parents: &HashMap<u32, u32>,
    ) -> bool {
        if ancestor == candidate {
            return true;
        }
        let mut visited = HashSet::new();
        for _ in 0..MAX_ANCESTORS {
            if !visited.insert(candidate) {
                return false;
            }
            let Some(&parent) = parents.get(&candidate) else {
                return false;
            };
            if parent == ancestor {
                return true;
            }
            candidate = parent;
        }
        false
    }

    #[test]
    fn steam_rejects_wrapper_executables() {
        let mut steam = app("steam.desktop", "Steam");
        steam.executable = Some("flatpak".to_owned());
        let error = steam_executable(&steam).unwrap_err().to_string();
        assert!(error.contains("unsupported") || error.contains("not an executable"));
    }
}
