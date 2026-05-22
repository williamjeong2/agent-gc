use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
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
}

pub fn default_roots() -> Vec<PathBuf> {
    let home = home_dir();
    [
        ".codex/worktrees",
        ".claude",
        ".opencode",
        ".config/opencode",
        ".cache/opencode",
        "dev",
        "workspace",
        "projects",
    ]
    .into_iter()
    .map(|p| home.join(p))
    .collect()
}

pub fn scan(paths: &[PathBuf]) -> Result<Vec<Artifact>> {
    let mut artifacts = Vec::new();
    scan_inner(paths, |artifact| artifacts.push(artifact))?;
    artifacts.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    Ok(artifacts)
}

pub fn scan_to_channel(paths: Vec<PathBuf>, sender: Sender<ScanEvent>) {
    let _ = scan_inner_with_progress(
        &paths,
        |artifact| {
            let _ = sender.send(ScanEvent::Artifact(artifact));
        },
        |progress| {
            let _ = sender.send(ScanEvent::Progress(progress.clone()));
        },
    );
    let _ = sender.send(ScanEvent::Done);
}

fn scan_inner<F>(paths: &[PathBuf], on_artifact: F) -> Result<()>
where
    F: FnMut(Artifact),
{
    scan_inner_with_progress(paths, on_artifact, |_| {})
}

fn scan_inner_with_progress<F, P>(
    paths: &[PathBuf],
    mut on_artifact: F,
    mut on_progress: P,
) -> Result<()>
where
    F: FnMut(Artifact),
    P: FnMut(&ScanProgress),
{
    let roots = if paths.is_empty() {
        default_roots()
    } else {
        paths.to_vec()
    };
    let names = artifact_names();

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
        progress.current_path = dir.clone();
        progress.pending_search_tasks = pending.len() as u64;
        progress.completed_search_tasks += 1;
        on_progress(&progress);

        let name = dir.file_name().map(|name| name.to_string_lossy());
        if name.as_deref().is_some_and(|name| names.contains(name)) {
            progress.pending_stats_calculations += 1;
            on_progress(&progress);
            let artifact = classify_artifact(&dir)
                .with_context(|| format!("failed to classify artifact {}", dir.display()))?;
            on_artifact(artifact);
            progress.pending_stats_calculations =
                progress.pending_stats_calculations.saturating_sub(1);
            progress.completed_stats_calculations += 1;
            on_progress(&progress);
            continue;
        }

        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                pending.push(path);
            }
        }
        progress.pending_search_tasks = pending.len() as u64;
        on_progress(&progress);
    }
    Ok(())
}

pub fn selected_by_preset(artifacts: &[Artifact], preset: Preset) -> Vec<Artifact> {
    artifacts
        .iter()
        .filter(|artifact| match preset {
            Preset::Safe => artifact.risk_level == RiskLevel::Safe,
        })
        .cloned()
        .collect()
}

pub fn remove_artifacts(artifacts: &[Artifact]) -> Result<u64> {
    let mut removed = 0;
    for artifact in artifacts {
        if !artifact.can_delete() {
            continue;
        }
        if artifact.path.exists() {
            fs::remove_dir_all(&artifact.path)
                .with_context(|| format!("failed to delete {}", artifact.path.display()))?;
            removed += artifact.size;
        }
    }
    Ok(removed)
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

fn trim_decimal(mut text: String) -> String {
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

fn classify_artifact(path: &Path) -> Result<Artifact> {
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
    let git_status_clean = git_status_clean(&project_root);
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
    let path_text = path.to_string_lossy();
    if path_text.contains("/.claude/")
        || path_text.contains("/.opencode/")
        || path_text.contains("/.config/opencode/")
        || path_text.contains("/.cache/opencode/")
    {
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
        {
            return current;
        }
        if !current.pop() {
            return path.parent().unwrap_or(path).to_path_buf();
        }
    }
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
    path.to_string_lossy().contains("/.codex/worktrees/")
}

fn is_agent_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.contains("/.codex/")
        || text.contains("/.claude/")
        || text.contains("/.opencode/")
        || text.contains("/.config/opencode/")
        || text.contains("/.cache/opencode/")
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
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
    fn dangerous_file_inside_artifact_locks_candidate() {
        let root = temp_root("inside-danger");
        let artifact = root.join("app/build");
        fs::create_dir_all(&artifact).unwrap();
        fs::write(artifact.join("local.sqlite"), "db").unwrap();

        let results = scan(std::slice::from_ref(&root)).unwrap();
        let build = results
            .iter()
            .find(|artifact| artifact.path == root.join("app/build"))
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
    fn formats_sizes_with_decimal_units() {
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_000), "1 KB");
        assert_eq!(human_size(1_024), "1.02 KB");
        assert_eq!(human_size(19_290_000_000), "19.3 GB");
        assert_eq!(human_size(784_820_000), "785 MB");
    }

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("agent-gc-{name}-{nanos}"))
    }
}
