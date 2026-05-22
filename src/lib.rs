mod scanner;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use scanner::{human_size, remove_artifacts, scan, selected_by_preset, total_size, Preset};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "agent-gc")]
#[command(about = "AI agent worktree and dev artifact garbage collector")]
#[command(version)]
struct Cli {
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

    /// Cleanup preset to apply.
    #[arg(long, value_enum, default_value_t = CleanPreset::Safe)]
    preset: CleanPreset,

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

pub fn run() -> Result<()> {
    let args = std::iter::once("agent-gc".to_string()).chain(std::env::args().skip(1));
    let cli = Cli::parse_from(args);
    match cli.command {
        Some(Command::Scan(args)) => run_scan(args),
        Some(Command::Clean(args)) => run_clean(args),
        None => tui::run(Vec::new()),
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

fn run_clean(args: CleanArgs) -> Result<()> {
    let artifacts = scan(&args.paths)?;
    let mut selected = selected_by_preset(
        &artifacts,
        match args.preset {
            CleanPreset::Safe => Preset::Safe,
        },
    );
    selected.retain(|artifact| {
        category_matches(args.category, &artifact.category)
            && min_size_matches(args.min_size, artifact.size)
            && artifact.can_delete()
    });

    if selected.is_empty() {
        println!("No matching SAFE artifacts found.");
        return Ok(());
    }

    if args.dry_run {
        println!("Would delete {} SAFE artifacts:", selected.len());
        println!();
        print_artifact_table(&selected);
        println!();
        println!("Total reclaimable: {}", human_size(total_size(&selected)));
        println!("DANGER and CAUTION candidates are excluded from this preset.");
        return Ok(());
    }

    println!(
        "Delete {} SAFE artifacts and reclaim {}?",
        selected.len(),
        human_size(total_size(&selected))
    );
    println!();
    print_artifact_table(&selected);
    println!();
    println!("This action will remove generated dependencies/build artifacts.");
    println!("DANGER and CAUTION candidates are excluded from this preset.");
    print!("[y/N] ");
    io::stdout().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        println!("Cancelled.");
        return Ok(());
    }

    let removed = remove_artifacts(&selected)?;
    println!("Deleted {}.", human_size(removed));
    Ok(())
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

fn parse_size(input: &str) -> std::result::Result<u64, String> {
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
