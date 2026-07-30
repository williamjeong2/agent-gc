mod scanner;
mod tui;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use scanner::{
    human_size, parse_duration_days, parse_size, remove_artifacts, scan, selected_by_preset,
    total_size, Preset,
};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "agent-gc")]
#[command(about = "AI agent worktree and dev artifact garbage collector")]
#[command(version)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    /// Paths to scan in TUI mode. Defaults to common agent and project directories.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Scan for cleanup candidates")]
    Scan(ScanArgs),
    #[command(about = "Preview or delete cleanup candidates")]
    Clean(CleanArgs),
}

#[derive(Parser)]
struct ScanArgs {
    /// Paths to scan. Defaults to common agent and project directories.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Filter by candidate category.
    #[arg(long, value_enum)]
    category: Option<CategoryArg>,

    /// Filter by risk level.
    #[arg(long, value_enum)]
    risk: Option<RiskArg>,

    /// Only show candidates at least this large. Examples: 500MB, 1.5GB, 200K.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    min_size: Option<u64>,
}

#[derive(Parser)]
struct CleanArgs {
    /// Paths to scan. Defaults to common agent and project directories.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Show what would be deleted without deleting anything.
    #[arg(long)]
    dry_run: bool,

    /// Skip the interactive confirmation prompt.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Cleanup preset to apply.
    #[arg(long, value_enum, default_value_t = CleanPreset::Safe)]
    preset: CleanPreset,

    /// Minimum age for --preset older. Examples: 30d, 7. Default: 30d.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration_days, default_value = "30d")]
    older_than: u64,

    /// Filter by candidate category.
    #[arg(long, value_enum)]
    category: Option<CategoryArg>,

    /// Only delete candidates at least this large. Examples: 500MB, 1.5GB, 200K.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    min_size: Option<u64>,
}

#[derive(Clone, Copy, ValueEnum)]
enum CleanPreset {
    Safe,
    AgentOnly,
    Older,
}

#[derive(Clone, Copy, ValueEnum)]
enum CategoryArg {
    Agent,
    AgentCache,
    Node,
    Python,
    Rust,
    Cache,
    Other,
}

#[derive(Clone, Copy, ValueEnum)]
enum RiskArg {
    Safe,
    Caution,
    Danger,
}

pub fn run() -> Result<ExitCode> {
    let args = std::iter::once("agent-gc".to_string()).chain(std::env::args().skip(1));
    let cli = Cli::parse_from(args);
    match cli.command {
        Some(Command::Scan(args)) => {
            run_scan(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Clean(args)) => run_clean(args),
        None => {
            tui::run(cli.paths)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn run_scan(args: ScanArgs) -> Result<()> {
    let mut artifacts = scan(&args.paths)?;
    artifacts.retain(|artifact| {
        category_matches(args.category, &artifact.category)
            && risk_matches(args.risk, artifact.risk_level.label())
            && min_size_matches(args.min_size, artifact.size)
    });

    if args.json {
        println!("{}", serde_json::to_string_pretty(&artifacts)?);
        return Ok(());
    }

    println!("agent-gc scan");
    println!(
        "Found: {}   Releasable: {}",
        artifacts.len(),
        human_size(total_size(&artifacts))
    );
    println!();
    print_artifact_table(&artifacts);
    Ok(())
}

fn run_clean(args: CleanArgs) -> Result<ExitCode> {
    let artifacts = scan(&args.paths)?;
    let preset = match args.preset {
        CleanPreset::Safe => Preset::Safe,
        CleanPreset::AgentOnly => Preset::AgentOnly,
        CleanPreset::Older => Preset::Older {
            days: args.older_than,
        },
    };
    let mut selected = selected_by_preset(&artifacts, preset);
    selected.retain(|artifact| {
        category_matches(args.category, &artifact.category)
            && min_size_matches(args.min_size, artifact.size)
            && artifact.can_delete()
    });

    if selected.is_empty() {
        println!("No matching SAFE artifacts found for this preset.");
        return Ok(ExitCode::SUCCESS);
    }

    let preset_label = match args.preset {
        CleanPreset::Safe => "safe".to_string(),
        CleanPreset::AgentOnly => "agent-only".to_string(),
        CleanPreset::Older => format!("older than {}d", args.older_than),
    };

    if args.dry_run {
        println!(
            "Would delete {} SAFE artifacts (preset: {}):",
            selected.len(),
            preset_label
        );
        println!();
        print_artifact_table(&selected);
        println!();
        println!("Total reclaimable: {}", human_size(total_size(&selected)));
        println!("DANGER and CAUTION candidates are excluded from bulk presets.");
        return Ok(ExitCode::SUCCESS);
    }

    if !args.yes {
        if !io::stdin().is_terminal() {
            bail!("refusing non-interactive delete without --yes (use --dry-run to preview)");
        }
        println!(
            "Delete {} SAFE artifacts (preset: {}) and reclaim {}?",
            selected.len(),
            preset_label,
            human_size(total_size(&selected))
        );
        println!();
        print_artifact_table(&selected);
        println!();
        println!("This action will remove generated dependencies/build artifacts.");
        println!("DANGER and CAUTION candidates are excluded from bulk presets.");
        print!("[y/N] ");
        io::stdout().flush()?;

        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            println!("Cancelled.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let report = remove_artifacts(&selected);
    println!(
        "Deleted {} from {} item(s).",
        human_size(report.bytes_removed),
        report.deleted.len()
    );
    if report.failed.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    println!("Failed {} item(s):", report.failed.len());
    for (path, error) in &report.failed {
        println!("  {} ({error})", path.display());
    }
    Ok(ExitCode::from(1))
}

fn print_artifact_table(artifacts: &[scanner::Artifact]) {
    if artifacts.is_empty() {
        println!("No matching artifacts found.");
        return;
    }
    println!(
        "{:<7} {:>10} {:<12} {:<8} {:<18} Path",
        "Risk", "Size", "Category", "Git", "Project"
    );
    for artifact in artifacts {
        let git = match artifact.git_status_clean {
            Some(true) => "clean",
            Some(false) => "dirty",
            None => "no-git",
        };
        println!(
            "{:<7} {:>10} {:<12} {:<8} {:<18} {}",
            artifact.risk_level.label(),
            artifact.size_human,
            artifact.category,
            git,
            truncate(
                &artifact
                    .project_name
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
                18
            ),
            artifact.path.display()
        );
    }
}

fn category_matches(filter: Option<CategoryArg>, category: &str) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    matches!(
        (filter, category),
        (CategoryArg::Agent, "AGENT")
            | (CategoryArg::AgentCache, "AGENT_CACHE")
            | (CategoryArg::Node, "NODE")
            | (CategoryArg::Python, "PY")
            | (CategoryArg::Rust, "RUST")
            | (CategoryArg::Cache, "CACHE")
            | (CategoryArg::Other, "OTHER")
    )
}

fn risk_matches(filter: Option<RiskArg>, risk: &str) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    matches!(
        (filter, risk),
        (RiskArg::Safe, "SAFE") | (RiskArg::Caution, "CAUTION") | (RiskArg::Danger, "DANGER")
    )
}

fn min_size_matches(min_size: Option<u64>, size: u64) -> bool {
    min_size.is_none_or(|min_size| size >= min_size)
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut output = value.chars().take(width - 3).collect::<String>();
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_filter_distinguishes_agent_and_cache() {
        assert!(category_matches(Some(CategoryArg::Agent), "AGENT"));
        assert!(!category_matches(Some(CategoryArg::Agent), "AGENT_CACHE"));
        assert!(category_matches(
            Some(CategoryArg::AgentCache),
            "AGENT_CACHE"
        ));
        assert!(!category_matches(Some(CategoryArg::Cache), "AGENT_CACHE"));
        assert!(category_matches(Some(CategoryArg::Cache), "CACHE"));
    }
}
