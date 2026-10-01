//! wspm: workspace package manager CLI.
//!
//! Phase 0: manifest discovery/parsing (`resolve` prints the normalized
//! dependency list). Other commands are wired up but not yet implemented.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use wspm_core::{ManifestEntry, discover_manifest, load_manifest};

#[derive(Parser, Debug)]
#[command(name = "wspm", version, about = "Workspace package manager for .repos/.rosinstall workspaces")]
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
    /// Resolve dependencies recursively and print the normalized list.
    Resolve,
    /// Fetch, place and version-sync every repository (needs lockfile).
    Sync,
    /// Re-resolve (and update) one repository or everything.
    Update {
        path: Option<String>,
    },
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

fn load(cli: &Cli) -> Result<wspm_core::Manifest> {
    let dir = workspace_dir(cli)?;
    let manifest_path = match &cli.manifest {
        Some(p) => p.clone(),
        None => discover_manifest(&dir).map_err(anyhow::Error::from)?,
    };
    load_manifest(&manifest_path).map_err(anyhow::Error::from)
}

fn cmd_resolve(cli: &Cli) -> Result<()> {
    let manifest = load(cli)?;
    println!("# from {}", manifest.source.display());
    for entry in &manifest.entries {
        match entry {
            ManifestEntry::Repo(spec) => {
                let sparse = if spec.subpaths.is_empty() {
                    String::new()
                } else {
                    format!("  sparse=[{}]", spec.subpaths.join(","))
                };
                println!(
                    "{} [{}] {} @ {}{}",
                    spec.path.display(),
                    spec.kind,
                    spec.normalized_url(),
                    spec.version,
                    sparse
                );
            }
            ManifestEntry::Other { local_name } => {
                println!("# other: {local_name} (no VCS action)");
            }
            ManifestEntry::SetupFile { local_name } => {
                println!("# setup-file: {local_name} (no VCS action)");
            }
        }
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
