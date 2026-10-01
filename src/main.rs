//! ir: workspace package manager CLI.
//!
//! `resolve` recursively resolves the workspace manifest and writes `ir.lock`.
//! Other commands are wired up but not yet implemented.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ir_core::{
    discover_manifest, load_manifest, Cache, Resolver, SyncOptions, SyncStatus, LOCKFILE_NAME,
};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "ir",
    version,
    about = "Workspace package manager for .repos/.rosinstall workspaces"
)]
struct Cli {
    /// Run as if started in <DIR> instead of the current directory.
    #[arg(short = 'C', long, global = true)]
    dir: Option<PathBuf>,

    /// Explicit manifest file (overrides discovery of .repos/.rosinstall).
    #[arg(long, global = true)]
    manifest: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create an empty workspace manifest.
    Init,
    /// Resolve dependencies recursively and write ir.lock.
    Resolve,
    /// Fetch, place and version-sync every repository (needs ir.lock).
    Sync {
        /// Parallel jobs (default: number of CPUs).
        #[arg(long)]
        jobs: Option<usize>,
        /// Replace existing checkouts whose state differs from the lockfile.
        #[arg(long)]
        force: bool,
    },
    /// Re-resolve (and update) one repository or everything.
    Update { path: Option<String> },
    /// Show sync status of the workspace.
    Status,
    /// Export the workspace state as a .repos file (vcstool compatible).
    Export {
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long)]
        exact: bool,
    },
    /// Add a repository to the manifest.
    Add {
        url: String,
        #[arg(long)]
        path: Option<String>,
    },
    /// Remove materialized repositories that are no longer declared.
    Prune,
    /// Inspect and clean the cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
}

#[derive(Subcommand, Debug)]
enum CacheAction {
    Clean,
    Gc,
}

fn workspace_dir(cli: &Cli) -> Result<PathBuf> {
    match &cli.dir {
        Some(d) => Ok(d.clone()),
        None => std::env::current_dir().context("cannot determine current directory"),
    }
}

fn load(cli: &Cli) -> Result<ir_core::Manifest> {
    let dir = workspace_dir(cli)?;
    let manifest_path = match &cli.manifest {
        Some(p) => p.clone(),
        None => discover_manifest(&dir).map_err(anyhow::Error::from)?,
    };
    load_manifest(&manifest_path).map_err(anyhow::Error::from)
}

fn cmd_sync(cli: &Cli, jobs: Option<usize>, force: bool) -> Result<()> {
    let dir = workspace_dir(cli)?;
    let lock_path = dir.join(LOCKFILE_NAME);
    if !lock_path.is_file() {
        anyhow::bail!("no {} found; run `ir resolve` first", lock_path.display());
    }
    let locked = ir_core::read_lock(&lock_path).map_err(anyhow::Error::from)?;
    let cache = Cache::new().map_err(anyhow::Error::from)?;
    let jobs = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });
    let progress = std::io::stderr().is_terminal();
    let opts = SyncOptions {
        jobs,
        force,
        progress,
    };
    let outcomes = ir_core::sync_workspace(&dir, &cache, &locked, &opts);
    let (mut synced, mut up_to_date, mut skipped, mut failed) = (0, 0, 0, 0);
    for o in &outcomes {
        match &o.status {
            SyncStatus::UpToDate => up_to_date += 1,
            SyncStatus::Synced => synced += 1,
            SyncStatus::Skipped(m) => {
                skipped += 1;
                println!("○ {}: {m}", o.path.display());
            }
            SyncStatus::Failed(e) => {
                failed += 1;
                println!("✗ {}: {e}", o.path.display());
            }
        }
    }
    println!("synced {synced}, up-to-date {up_to_date}, skipped {skipped}, failed {failed}");
    if failed > 0 {
        anyhow::bail!("{failed} repositories failed to sync");
    }
    Ok(())
}

fn cmd_resolve(cli: &Cli) -> Result<()> {
    let dir = workspace_dir(cli)?;
    let manifest = load(cli)?;
    let cache = Cache::new().map_err(anyhow::Error::from)?;
    let repos = Resolver::new(&cache)
        .resolve(&manifest)
        .map_err(anyhow::Error::from)?;
    let lock_path = dir.join(LOCKFILE_NAME);
    ir_core::write_lock(&lock_path, &repos).map_err(anyhow::Error::from)?;
    println!(
        "# resolved {} repositories -> {}",
        repos.len(),
        lock_path.display()
    );
    for r in &repos {
        let indent = "  ".repeat(r.depth);
        println!(
            "{indent}{} [{}] {} @ {} -> {}",
            r.spec.path.display(),
            r.spec.kind,
            r.spec.normalized_url(),
            r.spec.version,
            r.pin()
        );
    }
    Ok(())
}

fn not_yet(name: &str) -> Result<()> {
    println!("{name}: not yet implemented (planned for a later phase)");
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Init => not_yet("init"),
        Command::Resolve => cmd_resolve(&cli),
        Command::Sync { jobs, force } => cmd_sync(&cli, *jobs, *force),
        Command::Update { .. } => not_yet("update"),
        Command::Status => not_yet("status"),
        Command::Export { .. } => not_yet("export"),
        Command::Add { .. } => not_yet("add"),
        Command::Prune => not_yet("prune"),
        Command::Cache { .. } => not_yet("cache"),
    }
}
