use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::time::SystemTime;
use walkdir::WalkDir;

const CATEGORY_AGENT: &str = "AGENT";
const CATEGORY_AGENT_CACHE: &str = "AGENT_CACHE";
const CATEGORY_NODE: &str = "NODE";
const CATEGORY_PYTHON: &str = "PY";
const CATEGORY_RUST: &str = "RUST";
const CATEGORY_CACHE: &str = "CACHE";
const CATEGORY_OTHER: &str = "OTHER";

const MARKER_SEARCH_DEPTH: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskLevel {
    Safe,
    Caution,
    Danger,
}

impl RiskLevel {
    pub fn label(self) -> &'static str {
        match self {
            RiskLevel::Safe => "SAFE",
            RiskLevel::Caution => "CAUTION",
            RiskLevel::Danger => "DANGER",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            RiskLevel::Danger => 0,
            RiskLevel::Caution => 1,
            RiskLevel::Safe => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub size: u64,
    pub size_human: String,
    pub last_modified: Option<DateTime<Utc>>,
    pub category: String,
    pub risk_level: RiskLevel,
    pub reason: String,
    pub project_name: Option<String>,
    pub is_agent_worktree: bool,
    pub git_status_clean: Option<bool>,
    pub dangerous_files_detected: Vec<PathBuf>,
}

impl Artifact {
    pub fn can_delete(&self) -> bool {
        self.risk_level != RiskLevel::Danger && self.dangerous_files_detected.is_empty()
    }
}

#[derive(Debug)]
pub enum ScanEvent {
    Progress(ScanProgress),
    Artifact(Artifact),
    Error(String),
    Done,
}

#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub current_path: PathBuf,
    pub completed_search_tasks: u64,
    pub pending_search_tasks: u64,
    pub completed_stats_calculations: u64,
    pub pending_stats_calculations: u64,
}

impl ScanProgress {
    pub fn scanned_dirs(&self) -> u64 {
        self.completed_search_tasks
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Safe,
    AgentOnly,
    Older { days: u64 },
}

#[derive(Debug, Default)]
pub struct RemoveReport {
    pub deleted: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    pub bytes_removed: u64,
    pub skipped: Vec<PathBuf>,
}

pub fn default_roots() -> Vec<PathBuf> {
    let home = home_dir();
    let mut roots = Vec::new();

    // Agent homes / caches (Path::join segments so Windows separators stay correct)
    for parts in [
        &[".codex", "worktrees"][..],
        &[".claude"][..],
        &[".opencode"][..],
        &[".cursor"][..],
        &[".gemini"][..],
        &[".aider"][..],
        &[".orca"][..],
    ] {
        roots.push(join_parts(&home, parts));
    }

    // XDG config/cache when set, otherwise ~/.config and ~/.cache
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| join_parts(&home, &[".config"]));
    roots.push(join_parts(&config_home, &["opencode"]));

    let cache_home = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| join_parts(&home, &[".cache"]));
    roots.push(join_parts(&cache_home, &["opencode"]));

    for parts in [
        ["dev"],
        ["workspace"],
        ["projects"],
        ["Developer"],
        ["src"],
        ["orca"],
    ] {
        roots.push(join_parts(&home, &parts));
    }

    // Windows-style user project folders when present
    if cfg!(windows) {
        for parts in [
            ["source", "repos"],
            ["Documents", "GitHub"],
            ["Documents", "Projects"],
        ] {
            roots.push(join_parts(&home, &parts));
        }
    }

    roots
}

pub fn scan(paths: &[PathBuf]) -> Result<Vec<Artifact>> {
    let mut artifacts = Vec::new();
    scan_inner(paths, |artifact| artifacts.push(artifact))?;
    artifacts.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    Ok(artifacts)
}

pub fn scan_to_channel(paths: Vec<PathBuf>, sender: Sender<ScanEvent>) {
    if let Err(error) = scan_inner_with_progress(
        &paths,
        |artifact| {
            let _ = sender.send(ScanEvent::Artifact(artifact));
        },
        |progress| {
            let _ = sender.send(ScanEvent::Progress(progress.clone()));
        },
        |message| {
            let _ = sender.send(ScanEvent::Error(message));
        },
    ) {
        let _ = sender.send(ScanEvent::Error(format!("scan failed: {error:#}")));
    }
    let _ = sender.send(ScanEvent::Done);
}

fn scan_inner<F>(paths: &[PathBuf], on_artifact: F) -> Result<()>
where
    F: FnMut(Artifact),
{
    scan_inner_with_progress(paths, on_artifact, |_| {}, |_| {})
}

fn scan_inner_with_progress<F, P, E>(
    paths: &[PathBuf],
    mut on_artifact: F,
    mut on_progress: P,
    mut on_error: E,
) -> Result<()>
where
    F: FnMut(Artifact),
    P: FnMut(&ScanProgress),
    E: FnMut(String),
{
    let roots = if paths.is_empty() {
        default_roots()
    } else {
        paths.to_vec()
    };
    let names = artifact_names();
    let mut git_cache: HashMap<PathBuf, Option<bool>> = HashMap::new();

    let mut pending = roots
        .into_iter()
        .filter(|root| root.exists() && root.is_dir())
        .collect::<Vec<_>>();
    let mut progress = ScanProgress {
        current_path: pending.last().cloned().unwrap_or_default(),
        completed_search_tasks: 0,
        pending_search_tasks: pending.len() as u64,
        completed_stats_calculations: 0,
        pending_stats_calculations: 0,
    };

    while let Some(dir) = pending.pop() {
        if should_skip_dir(&dir) {
            continue;
        }

        progress.current_path = dir.clone();
        progress.pending_search_tasks = pending.len() as u64;
        progress.completed_search_tasks += 1;
        on_progress(&progress);

        let name = dir.file_name().map(|name| name.to_string_lossy());
        if name
            .as_deref()
            .is_some_and(|name| names.contains(name) && is_valid_artifact_dir(&dir, name))
        {
            progress.pending_stats_calculations += 1;
            on_progress(&progress);
            match classify_artifact(&dir, &mut git_cache) {
                Ok(artifact) => on_artifact(artifact),
                Err(error) => on_error(format!("failed to classify {}: {error:#}", dir.display())),
            }
            progress.pending_stats_calculations =
                progress.pending_stats_calculations.saturating_sub(1);
            progress.completed_stats_calculations += 1;
            on_progress(&progress);
            continue;
        }

        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                on_error(format!("cannot read {}: {error}", dir.display()));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir())
                && !should_skip_dir(&path)
            {
                pending.push(path);
            }
        }
        progress.pending_search_tasks = pending.len() as u64;
        on_progress(&progress);
    }
    Ok(())
}

pub fn selected_by_preset(artifacts: &[Artifact], preset: Preset) -> Vec<Artifact> {
    let now = Utc::now();
    artifacts
        .iter()
        .filter(|artifact| match preset {
            Preset::Safe => artifact.risk_level == RiskLevel::Safe,
            Preset::AgentOnly => {
                artifact.risk_level == RiskLevel::Safe
                    && (artifact.is_agent_worktree
                        || artifact.category == CATEGORY_AGENT
                        || artifact.category == CATEGORY_AGENT_CACHE)
            }
            Preset::Older { days } => {
                artifact.risk_level == RiskLevel::Safe
                    && artifact
                        .last_modified
                        .is_some_and(|modified| now - modified >= Duration::days(days as i64))
            }
        })
        .cloned()
        .collect()
}

pub fn remove_artifacts(artifacts: &[Artifact]) -> RemoveReport {
    let mut report = RemoveReport::default();
    for artifact in artifacts {
        if !artifact.can_delete() {
            report.skipped.push(artifact.path.clone());
            continue;
        }
        if !artifact.path.exists() {
            report
                .failed
                .push((artifact.path.clone(), "path no longer exists".to_string()));
            continue;
        }

        let measured = dir_size(&artifact.path);
        let bytes = if measured > 0 {
            measured
        } else {
            artifact.size
        };

        match fs::remove_dir_all(&artifact.path) {
            Ok(()) => {
                report.deleted.push(artifact.path.clone());
                report.bytes_removed = report.bytes_removed.saturating_add(bytes);
            }
            Err(error) => {
                report
                    .failed
                    .push((artifact.path.clone(), error.to_string()));
            }
        }
    }
    report
}

pub fn total_size(artifacts: &[Artifact]) -> u64 {
    artifacts.iter().map(|artifact| artifact.size).sum()
}

pub fn human_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if size < 1000 {
        return format!("{size} B");
    }

    let mut value = size as f64;
    let mut unit_index = 0;
    while value >= 1000.0 && unit_index < UNITS.len() - 1 {
        value /= 1000.0;
        unit_index += 1;
    }

    let text = if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        trim_decimal(format!("{value:.1}"))
    } else {
        trim_decimal(format!("{value:.2}"))
    };
    format!("{} {}", text, UNITS[unit_index])
}

pub fn parse_size(input: &str) -> std::result::Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("size cannot be empty".to_string());
    }

    let split_at = trimmed
        .find(|character: char| !(character.is_ascii_digit() || character == '.'))
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split_at);
    let value = number
        .parse::<f64>()
        .map_err(|_| format!("invalid size: {input}"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid size: {input}"));
    }

    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_000.0,
        "m" | "mb" => 1_000_000.0,
        "g" | "gb" => 1_000_000_000.0,
        "t" | "tb" => 1_000_000_000_000.0,
        _ => return Err(format!("unsupported size unit: {unit}")),
    };

    Ok((value * multiplier).round() as u64)
}

pub fn parse_duration_days(input: &str) -> std::result::Result<u64, String> {
    let trimmed = input.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Err("duration cannot be empty".to_string());
    }
    if let Some(number) = trimmed.strip_suffix('d') {
        return number
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("invalid duration: {input}"));
    }
    if let Ok(days) = trimmed.parse::<u64>() {
        return Ok(days);
    }
    Err(format!("invalid duration (use e.g. 30d): {input}"))
}

fn trim_decimal(mut text: String) -> String {
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

fn classify_artifact(
    path: &Path,
    git_cache: &mut HashMap<PathBuf, Option<bool>>,
) -> Result<Artifact> {
    let size = dir_size(path);
    let metadata = fs::metadata(path).ok();
    let last_modified = metadata
        .and_then(|metadata| metadata.modified().ok())
        .map(system_time_to_utc);
    let project_root = project_root_for(path);
    let dangerous_files_detected = detect_dangerous_files(path);
    let is_agent_worktree = is_agent_worktree_path(path);
    let category = category_for(path, is_agent_worktree);
    let mut risk_level = risk_for(path);
    if !dangerous_files_detected.is_empty() {
        risk_level = RiskLevel::Danger;
    }
    let git_status_clean = cached_git_status_clean(&project_root, git_cache);
    let reason = reason_for(path, risk_level, is_agent_worktree);

    Ok(Artifact {
        path: path.to_path_buf(),
        size,
        size_human: human_size(size),
        last_modified,
        category,
        risk_level,
        reason,
        project_name: project_root
            .file_name()
            .map(|name| name.to_string_lossy().to_string()),
        is_agent_worktree,
        git_status_clean,
        dangerous_files_detected,
    })
}

fn artifact_names() -> HashSet<&'static str> {
    [
        "node_modules",
        ".next",
        ".turbo",
        "dist",
        "build",
        "coverage",
        ".vite",
        ".cache",
        ".venv",
        "venv",
        "env",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        "target",
    ]
    .into_iter()
    .collect()
}

fn is_valid_artifact_dir(path: &Path, name: &str) -> bool {
    match name {
        "env" => looks_like_python_venv(path),
        "build" | "dist" | "coverage" => has_project_marker_nearby(
            path,
            &[
                "package.json",
                "Cargo.toml",
                "pyproject.toml",
                "go.mod",
                "package-lock.json",
                "pnpm-lock.yaml",
                "yarn.lock",
                "bun.lock",
                "bun.lockb",
            ],
        ),
        "target" => has_ancestor_marker(path, "Cargo.toml"),
        _ => true,
    }
}

fn looks_like_python_venv(path: &Path) -> bool {
    path.join("bin/python").exists()
        || path.join("bin/python3").exists()
        || path.join("Scripts/python.exe").exists()
        || path.join("pyvenv.cfg").exists()
}

fn has_project_marker_nearby(path: &Path, markers: &[&str]) -> bool {
    let mut current = path.parent().unwrap_or(path).to_path_buf();
    for _ in 0..MARKER_SEARCH_DEPTH {
        for marker in markers {
            if current.join(marker).exists() {
                return true;
            }
        }
        if !current.pop() {
            break;
        }
    }
    false
}

fn has_ancestor_marker(path: &Path, marker: &str) -> bool {
    let mut current = path.parent().unwrap_or(path).to_path_buf();
    for _ in 0..MARKER_SEARCH_DEPTH {
        if current.join(marker).exists() {
            return true;
        }
        if !current.pop() {
            break;
        }
    }
    false
}

fn should_skip_dir(path: &Path) -> bool {
    match file_name(path).as_deref() {
        Some(".git" | ".svn" | ".hg" | ".jj" | ".Trash" | "lost+found") => true,
        // Windows recycle bin / system dirs
        Some(name) if name.eq_ignore_ascii_case("$RECYCLE.BIN") => true,
        Some(name) if name.eq_ignore_ascii_case("System Volume Information") => true,
        // Linux trash: ~/.local/share/Trash
        Some("Trash") => path_contains_sequence(path, &[".local", "share", "Trash"]),
        _ => false,
    }
}

fn risk_for(path: &Path) -> RiskLevel {
    match file_name(path).as_deref() {
        Some(".venv" | "venv" | "env") => RiskLevel::Caution,
        Some(".cache") if !is_agent_path(path) => RiskLevel::Caution,
        _ => RiskLevel::Safe,
    }
}

fn category_for(path: &Path, is_agent_worktree: bool) -> String {
    if is_agent_worktree {
        return CATEGORY_AGENT.to_string();
    }
    if is_agent_cache_path(path) {
        return CATEGORY_AGENT_CACHE.to_string();
    }
    match file_name(path).as_deref() {
        Some("node_modules" | ".next" | ".turbo" | "dist" | "build" | "coverage" | ".vite") => {
            CATEGORY_NODE.to_string()
        }
        Some(
            ".venv" | "venv" | "env" | "__pycache__" | ".pytest_cache" | ".mypy_cache"
            | ".ruff_cache",
        ) => CATEGORY_PYTHON.to_string(),
        Some("target") => CATEGORY_RUST.to_string(),
        Some(".cache") => CATEGORY_CACHE.to_string(),
        _ => CATEGORY_OTHER.to_string(),
    }
}

fn reason_for(path: &Path, risk: RiskLevel, is_agent_worktree: bool) -> String {
    if risk == RiskLevel::Danger {
        return "Dangerous local files were detected near this artifact".to_string();
    }
    if is_agent_worktree {
        return "Regenerable artifact inside an AI agent worktree".to_string();
    }
    match file_name(path).as_deref() {
        Some(".venv" | "venv" | "env") => {
            "Python virtual environment; reinstall cost may be non-trivial".to_string()
        }
        Some("node_modules") => "Reinstallable Node dependency directory".to_string(),
        Some("target") => "Regenerable Rust build output".to_string(),
        _ => "Regenerable build or tool cache artifact".to_string(),
    }
}

fn project_root_for(path: &Path) -> PathBuf {
    let mut current = path.parent().unwrap_or(path).to_path_buf();
    loop {
        if current.join(".git").exists()
            || current.join("package.json").exists()
            || current.join("Cargo.toml").exists()
            || current.join("pyproject.toml").exists()
            || current.join("go.mod").exists()
        {
            return current;
        }
        if !current.pop() {
            return path.parent().unwrap_or(path).to_path_buf();
        }
    }
}

fn cached_git_status_clean(
    path: &Path,
    cache: &mut HashMap<PathBuf, Option<bool>>,
) -> Option<bool> {
    if let Some(value) = cache.get(path) {
        return *value;
    }
    let value = git_status_clean(path);
    cache.insert(path.to_path_buf(), value);
    value
}

fn git_status_clean(path: &Path) -> Option<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("status")
        .arg("--porcelain")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout.is_empty())
}

fn detect_dangerous_files(search_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if !search_root.exists() {
        return found;
    }
    for entry in WalkDir::new(search_root)
        .follow_links(false)
        .max_depth(4)
        .into_iter()
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name();
        if is_dangerous_name(name) {
            found.push(entry.path().to_path_buf());
            if found.len() >= 10 {
                break;
            }
        }
    }
    found
}

fn is_dangerous_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    if is_env_allowlist(&name) {
        return false;
    }
    name == ".env"
        || name.starts_with(".env.")
        || matches!(
            name.as_ref(),
            "uploads" | "storage" | "certificates" | "secrets"
        )
        || name.ends_with(".sqlite")
        || name.ends_with(".sqlite3")
        || name.ends_with(".db")
        || name.ends_with(".pem")
        || name.ends_with(".key")
}

fn is_env_allowlist(name: &str) -> bool {
    matches!(
        name,
        ".env.example" | ".env.sample" | ".env.template" | ".env.test"
    ) || (name.starts_with(".env.") && name.ends_with(".example"))
}

fn dir_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if entry.file_type().is_file() {
                entry.metadata().ok().map(|metadata| metadata.len())
            } else {
                None
            }
        })
        .sum()
}

fn is_agent_worktree_path(path: &Path) -> bool {
    path_contains_sequence(path, &[".codex", "worktrees"])
        || path_contains_sequence(path, &[".cursor", "worktrees"])
        || path_contains_sequence(path, &["orca", "projects"])
        || path_contains_sequence(path, &["orca", "workspaces"])
}

fn is_agent_cache_path(path: &Path) -> bool {
    // Prefer specific sequences before broad ".cursor" so worktrees stay classified as AGENT.
    path_contains_sequence(path, &[".claude"])
        || path_contains_sequence(path, &[".opencode"])
        || path_contains_sequence(path, &[".config", "opencode"])
        || path_contains_sequence(path, &[".cache", "opencode"])
        || path_contains_sequence(path, &[".gemini"])
        || path_contains_sequence(path, &[".aider"])
        || path_contains_sequence(path, &[".orca"])
        || (path_contains_sequence(path, &[".cursor"])
            && !path_contains_sequence(path, &[".cursor", "worktrees"]))
}

fn is_agent_path(path: &Path) -> bool {
    path_contains_sequence(path, &[".codex"])
        || is_agent_cache_path(path)
        || is_agent_worktree_path(path)
}

/// True when `path` contains the consecutive normal components in `sequence`.
/// Separator-agnostic (works with `/` and `\` style paths built via Path APIs).
fn path_contains_sequence(path: &Path, sequence: &[&str]) -> bool {
    if sequence.is_empty() {
        return false;
    }
    let components = normal_path_components(path);
    if components.len() < sequence.len() {
        return false;
    }
    components.windows(sequence.len()).any(|window| {
        window
            .iter()
            .zip(sequence.iter())
            .all(|(component, expected)| component == expected)
    })
}

fn normal_path_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn join_parts(base: &Path, parts: &[&str]) -> PathBuf {
    let mut path = base.to_path_buf();
    for part in parts {
        path.push(part);
    }
    path
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
}

fn home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home);
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return PathBuf::from(profile);
    }
    if let (Some(drive), Some(path)) = (
        std::env::var_os("HOMEDRIVE").filter(|value| !value.is_empty()),
        std::env::var_os("HOMEPATH").filter(|value| !value.is_empty()),
    ) {
        let mut home = PathBuf::from(drive);
        home.push(path);
        return home;
    }
    PathBuf::from(".")
}

fn system_time_to_utc(time: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn scans_agent_artifacts_without_listing_agent_roots() {
        let root = temp_root("agent-artifacts");
        let codex_worktree = root.join(".codex/worktrees/91e7/app");
        let artifact = codex_worktree.join("node_modules/pkg");
        fs::create_dir_all(&artifact).unwrap();
        fs::write(artifact.join("index.js"), "x").unwrap();

        let results = scan(&[root.join(".codex/worktrees")]).unwrap();
        let paths: Vec<PathBuf> = results
            .iter()
            .map(|artifact| artifact.path.clone())
            .collect();

        assert!(paths.contains(&codex_worktree.join("node_modules")));
        assert!(!paths.contains(&root.join(".codex")));
        assert!(!paths.contains(&root.join(".codex/worktrees")));
        assert!(!paths.contains(&codex_worktree));
        assert_eq!(results[0].category, CATEGORY_AGENT);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adjacent_env_file_does_not_lock_regenerable_artifact() {
        let root = temp_root("adjacent-danger");
        let project = root.join("app");
        let artifact = project.join("node_modules/pkg");
        fs::create_dir_all(&artifact).unwrap();
        fs::write(project.join(".env"), "SECRET=1").unwrap();
        fs::write(artifact.join("index.js"), "x").unwrap();

        let results = scan(std::slice::from_ref(&root)).unwrap();
        let node_modules = results
            .iter()
            .find(|artifact| artifact.path == project.join("node_modules"))
            .unwrap();

        assert_eq!(node_modules.risk_level, RiskLevel::Safe);
        assert!(node_modules.dangerous_files_detected.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn env_example_inside_artifact_does_not_lock_candidate() {
        let root = temp_root("env-example");
        let project = root.join("app");
        let artifact = project.join("node_modules/pkg");
        fs::create_dir_all(&artifact).unwrap();
        fs::write(project.join("package.json"), "{}").unwrap();
        fs::write(artifact.join(".env.example"), "KEY=").unwrap();
        fs::write(artifact.join("index.js"), "x").unwrap();

        let results = scan(std::slice::from_ref(&root)).unwrap();
        let node_modules = results
            .iter()
            .find(|artifact| artifact.path == project.join("node_modules"))
            .unwrap();

        assert_eq!(node_modules.risk_level, RiskLevel::Safe);
        assert!(node_modules.dangerous_files_detected.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dangerous_file_inside_artifact_locks_candidate() {
        let root = temp_root("inside-danger");
        let project = root.join("app");
        let artifact = project.join("build");
        fs::create_dir_all(&artifact).unwrap();
        fs::write(project.join("package.json"), "{}").unwrap();
        fs::write(artifact.join("local.sqlite"), "db").unwrap();

        let results = scan(std::slice::from_ref(&root)).unwrap();
        let build = results
            .iter()
            .find(|artifact| artifact.path == project.join("build"))
            .unwrap();

        assert_eq!(build.risk_level, RiskLevel::Danger);
        assert_eq!(build.dangerous_files_detected.len(), 1);
        assert!(!build.can_delete());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn caution_artifacts_are_manually_deletable_when_unlocked() {
        let root = temp_root("caution-delete");
        let artifact = root.join("app/.venv/bin");
        fs::create_dir_all(&artifact).unwrap();
        fs::write(artifact.join("python"), "x").unwrap();

        let results = scan(std::slice::from_ref(&root)).unwrap();
        let venv = results
            .iter()
            .find(|artifact| artifact.path == root.join("app/.venv"))
            .unwrap();

        assert_eq!(venv.risk_level, RiskLevel::Caution);
        assert!(venv.can_delete());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opencode_cache_artifacts_are_agent_cache_not_plain_node() {
        let root = temp_root("opencode-cache");
        let artifact = root.join(".cache/opencode/packages/tool/node_modules/pkg");
        fs::create_dir_all(&artifact).unwrap();
        fs::write(artifact.join("index.js"), "x").unwrap();

        let results = scan(&[root.join(".cache/opencode")]).unwrap();
        let node_modules = results
            .iter()
            .find(|artifact| artifact.path.ends_with("node_modules"))
            .unwrap();

        assert_eq!(node_modules.category, CATEGORY_AGENT_CACHE);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bare_env_without_venv_markers_is_ignored() {
        let root = temp_root("bare-env");
        let env_dir = root.join("app/config/env");
        fs::create_dir_all(&env_dir).unwrap();
        fs::write(env_dir.join("settings.json"), "{}").unwrap();

        let results = scan(std::slice::from_ref(&root)).unwrap();
        assert!(results
            .iter()
            .all(|artifact| artifact.path != root.join("app/config/env")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn build_without_project_marker_is_ignored() {
        let root = temp_root("bare-build");
        let build = root.join("docs/build");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("notes.md"), "x").unwrap();

        let results = scan(std::slice::from_ref(&root)).unwrap();
        assert!(results
            .iter()
            .all(|artifact| artifact.path != root.join("docs/build")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn target_requires_cargo_toml() {
        let root = temp_root("target-marker");
        let unmarked = root.join("app/target");
        fs::create_dir_all(&unmarked).unwrap();
        fs::write(unmarked.join("file"), "x").unwrap();

        let marked_root = root.join("rust-app");
        let marked = marked_root.join("target/debug");
        fs::create_dir_all(&marked).unwrap();
        fs::write(marked_root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::write(marked.join("lib.rlib"), "x").unwrap();

        let results = scan(std::slice::from_ref(&root)).unwrap();
        let paths: Vec<_> = results.iter().map(|a| a.path.clone()).collect();
        assert!(!paths.contains(&unmarked));
        assert!(paths.contains(&marked_root.join("target")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_git_directories() {
        let root = temp_root("skip-git");
        let git_objects = root.join("app").join(".git").join("objects");
        fs::create_dir_all(&git_objects).unwrap();
        fs::write(git_objects.join("pack"), "x").unwrap();
        let nested = root
            .join("app")
            .join(".git")
            .join("node_modules")
            .join("pkg");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("index.js"), "x").unwrap();

        let results = scan(std::slice::from_ref(&root)).unwrap();
        assert!(results
            .iter()
            .all(|artifact| !path_contains_sequence(&artifact.path, &[".git"])));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_sequence_matching_is_separator_agnostic() {
        let worktree = PathBuf::from("Users")
            .join("me")
            .join(".codex")
            .join("worktrees")
            .join("ab12")
            .join("app")
            .join("node_modules");
        assert!(path_contains_sequence(&worktree, &[".codex", "worktrees"]));
        assert!(is_agent_worktree_path(&worktree));
        assert!(is_agent_path(&worktree));
        assert!(!is_agent_cache_path(&worktree));

        let cursor_cache = PathBuf::from("Users")
            .join("me")
            .join(".cursor")
            .join("projects")
            .join("x");
        assert!(is_agent_cache_path(&cursor_cache));
        assert!(!is_agent_worktree_path(&cursor_cache));

        let cursor_worktree = PathBuf::from("Users")
            .join("me")
            .join(".cursor")
            .join("worktrees")
            .join("x")
            .join("app");
        assert!(is_agent_worktree_path(&cursor_worktree));
        assert!(!is_agent_cache_path(&cursor_worktree));

        let orca_project = PathBuf::from("Users")
            .join("me")
            .join("orca")
            .join("projects")
            .join("app")
            .join("node_modules");
        assert!(is_agent_worktree_path(&orca_project));
        assert!(is_agent_path(&orca_project));
        assert!(!is_agent_cache_path(&orca_project));

        let orca_workspace = PathBuf::from("Users")
            .join("me")
            .join("orca")
            .join("workspaces")
            .join("app")
            .join("node_modules");
        assert!(is_agent_worktree_path(&orca_workspace));

        let orca_home = PathBuf::from("Users")
            .join("me")
            .join(".orca")
            .join("hooks");
        assert!(is_agent_cache_path(&orca_home));
        assert!(is_agent_path(&orca_home));
        assert!(!is_agent_worktree_path(&orca_home));
    }

    #[test]
    fn orca_project_artifacts_are_agent_worktrees_and_orca_home_is_agent_cache() {
        let root = temp_root("orca-targets");
        let project = root.join("orca/projects/app");
        let artifact = project.join("node_modules/pkg");
        fs::create_dir_all(&artifact).unwrap();
        fs::write(artifact.join("index.js"), "x").unwrap();

        let home_cache = root.join(".orca/hooks/node_modules/pkg");
        fs::create_dir_all(&home_cache).unwrap();
        fs::write(home_cache.join("index.js"), "x").unwrap();

        let results = scan(&[root.join("orca"), root.join(".orca")]).unwrap();
        let project_nm = results
            .iter()
            .find(|artifact| artifact.path == project.join("node_modules"))
            .unwrap();
        assert_eq!(project_nm.category, CATEGORY_AGENT);
        assert!(project_nm.is_agent_worktree);

        let home_nm = results
            .iter()
            .find(|artifact| artifact.path == root.join(".orca/hooks/node_modules"))
            .unwrap();
        assert_eq!(home_nm.category, CATEGORY_AGENT_CACHE);
        assert!(!home_nm.is_agent_worktree);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_roots_include_orca_paths() {
        let roots = default_roots();
        let home = home_dir();
        assert!(roots.contains(&join_parts(&home, &[".orca"])));
        assert!(roots.contains(&join_parts(&home, &["orca"])));
    }

    #[test]
    fn join_parts_builds_nested_paths() {
        let base = PathBuf::from("home");
        let path = join_parts(&base, &[".cache", "opencode"]);
        assert_eq!(path, base.join(".cache").join("opencode"));
        assert!(path_contains_sequence(&path, &[".cache", "opencode"]));
    }

    #[test]
    fn skips_linux_trash_and_recycle_bin_names() {
        let trash = PathBuf::from("home")
            .join(".local")
            .join("share")
            .join("Trash");
        assert!(should_skip_dir(&trash));

        let recycle = PathBuf::from("C:").join("$RECYCLE.BIN");
        assert!(should_skip_dir(&recycle));

        let normal_trash_name = PathBuf::from("project").join("Trash");
        assert!(!should_skip_dir(&normal_trash_name));
    }

    #[test]
    fn home_dir_prefers_home_then_userprofile() {
        // Just ensure the helper returns something non-empty-looking for this process.
        let home = home_dir();
        assert!(!home.as_os_str().is_empty());
    }

    #[test]
    fn default_roots_use_segment_joins_not_slash_literals() {
        let roots = default_roots();
        assert!(!roots.is_empty());
        // Every root should be absolute-ish under home or XDG, and never a single
        // component that still contains a path separator (Windows footgun).
        for root in &roots {
            let as_str = root.to_string_lossy();
            let last = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
            assert!(
                !last.contains('/') && !last.contains('\\'),
                "root last component should be a single segment: {as_str}"
            );
        }
    }

    #[test]
    fn partial_remove_reports_success_and_failure() {
        let root = temp_root("partial-remove");
        let keep = root.join("keep/node_modules");
        let gone = root.join("missing/node_modules");
        fs::create_dir_all(&keep).unwrap();
        fs::write(keep.join("a"), "x").unwrap();

        let artifacts = vec![
            Artifact {
                path: keep.clone(),
                size: 1,
                size_human: "1 B".into(),
                last_modified: None,
                category: CATEGORY_NODE.into(),
                risk_level: RiskLevel::Safe,
                reason: "test".into(),
                project_name: None,
                is_agent_worktree: false,
                git_status_clean: None,
                dangerous_files_detected: Vec::new(),
            },
            Artifact {
                path: gone.clone(),
                size: 2,
                size_human: "2 B".into(),
                last_modified: None,
                category: CATEGORY_NODE.into(),
                risk_level: RiskLevel::Safe,
                reason: "test".into(),
                project_name: None,
                is_agent_worktree: false,
                git_status_clean: None,
                dangerous_files_detected: Vec::new(),
            },
        ];

        let report = remove_artifacts(&artifacts);
        assert_eq!(report.deleted, vec![keep.clone()]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, gone);
        assert!(!keep.exists());
        assert!(report.bytes_removed >= 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preset_agent_only_filters_safe_agent_paths() {
        let agent = Artifact {
            path: PathBuf::from("/tmp/agent/node_modules"),
            size: 10,
            size_human: "10 B".into(),
            last_modified: None,
            category: CATEGORY_AGENT.into(),
            risk_level: RiskLevel::Safe,
            reason: "test".into(),
            project_name: None,
            is_agent_worktree: true,
            git_status_clean: None,
            dangerous_files_detected: Vec::new(),
        };
        let local = Artifact {
            path: PathBuf::from("/tmp/local/node_modules"),
            size: 20,
            size_human: "20 B".into(),
            last_modified: None,
            category: CATEGORY_NODE.into(),
            risk_level: RiskLevel::Safe,
            reason: "test".into(),
            project_name: None,
            is_agent_worktree: false,
            git_status_clean: None,
            dangerous_files_detected: Vec::new(),
        };
        let selected = selected_by_preset(&[agent.clone(), local], Preset::AgentOnly);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].path, agent.path);
    }

    #[test]
    fn formats_sizes_with_decimal_units() {
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_000), "1 KB");
        assert_eq!(human_size(1_024), "1.02 KB");
        assert_eq!(human_size(19_290_000_000), "19.3 GB");
        assert_eq!(human_size(784_820_000), "785 MB");
    }

    #[test]
    fn parses_size_units() {
        assert_eq!(parse_size("500MB").unwrap(), 500_000_000);
        assert_eq!(parse_size("1.5GB").unwrap(), 1_500_000_000);
        assert_eq!(parse_size("200K").unwrap(), 200_000);
        assert!(parse_size("").is_err());
        assert!(parse_size("10XB").is_err());
    }

    #[test]
    fn parses_duration_days() {
        assert_eq!(parse_duration_days("30d").unwrap(), 30);
        assert_eq!(parse_duration_days("7").unwrap(), 7);
        assert!(parse_duration_days("").is_err());
    }

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("agent-gc-{name}-{nanos}"))
    }
}
