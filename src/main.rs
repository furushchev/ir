//! ir: workspace package manager CLI.
//!
//! `resolve` recursively resolves the workspace manifest and writes `ir.lock`.
//! Other commands are wired up but not yet implemented.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ir_core::{discover_manifest, load_manifest, Cache, Resolver, LOCKFILE_NAME};
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
    /// Fetch, place and version-sync every repository (needs lockfile).
    Sync,
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
        Command::Sync => not_yet("sync"),
        Command::Update { .. } => not_yet("update"),
        Command::Status => not_yet("status"),
        Command::Export { .. } => not_yet("export"),
        Command::Add { .. } => not_yet("add"),
        Command::Prune => not_yet("prune"),
        Command::Cache { .. } => not_yet("cache"),
    }
}
