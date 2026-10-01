//! ir: workspace package manager CLI.

mod commands;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use commands::Format;
use ir_core::SourceKind;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum FormatArg {
    #[default]
    Text,
    Json,
}

impl From<FormatArg> for Format {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::Text => Format::Text,
            FormatArg::Json => Format::Json,
        }
    }
}

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

    /// Output format for resolve/sync/status/update.
    #[arg(long, global = true, value_enum, default_value = "text")]
    format: FormatArg,

    /// Total attempts for each network operation (fetch/pull/download).
    /// Also settable via IR_RETRIES.
    #[arg(long, global = true)]
    retries: Option<u32>,

    /// Skip network fetches for mirrors fetched more recently than this
    /// ("30s", "10m", "1h"; "0" = always fetch, the default).
    /// Also settable via IR_FETCH_INTERVAL.
    #[arg(long, global = true)]
    fetch_interval: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create an empty workspace manifest (.repos).
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
    /// Re-resolve and rewrite ir.lock, reporting pin changes.
    Update {
        /// Only update the subtree rooted at this manifest-declared path.
        path: Option<String>,
    },
    /// Show sync status of the workspace against ir.lock.
    Status,
    /// Export the workspace state as a .repos file (vcstool compatible).
    Export {
        /// Write to this file instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Pin exact revisions instead of the requested versions.
        #[arg(long)]
        exact: bool,
    },
    /// Add a repository to the manifest.
    Add {
        url: String,
        /// Workspace path (default: inferred from the URL).
        #[arg(long)]
        path: Option<String>,
        /// Repository kind (default: inferred from the URL).
        #[arg(long)]
        kind: Option<SourceKind>,
        /// Version: branch/tag/hash, integer revision, or archive subdir.
        #[arg(long)]
        version: Option<String>,
    },
    /// Remove materialized repositories that are no longer declared.
    Prune,
    /// Inspect and clean the cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Print shell completions.
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Subcommand, Debug)]
enum CacheAction {
    /// Remove everything from the cache.
    Clean,
    /// Remove cache entries not referenced by ir.lock.
    Gc,
}

fn workspace_dir(cli: &Cli) -> Result<PathBuf> {
    match &cli.dir {
        Some(d) => Ok(d.clone()),
        None => std::env::current_dir().context("cannot determine current directory"),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Network tuning flags feed the library through the environment so the
    // provider layer needs no extra plumbing.
    if let Some(n) = cli.retries {
        std::env::set_var("IR_RETRIES", n.max(1).to_string());
    }
    if let Some(ref d) = cli.fetch_interval {
        std::env::set_var("IR_FETCH_INTERVAL", d);
    }
    let dir = workspace_dir(&cli)?;
    let format = Format::from(cli.format);
    let jobs_default = || {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    };
    match &cli.command {
        Command::Init => commands::cmd_init(&dir),
        Command::Resolve => {
            let cache = ir_core::Cache::new().map_err(anyhow::Error::from)?;
            commands::cmd_resolve(&dir, cli.manifest.as_deref(), &cache, format)
        }
        Command::Sync { jobs, force } => {
            let cache = ir_core::Cache::new().map_err(anyhow::Error::from)?;
            commands::cmd_sync(
                &dir,
                &cache,
                jobs.unwrap_or_else(jobs_default),
                *force,
                format,
            )
        }
        Command::Update { path } => {
            let cache = ir_core::Cache::new().map_err(anyhow::Error::from)?;
            match path {
                Some(p) => commands::cmd_update_one(&dir, &cache, p, format),
                None => commands::cmd_update(&dir, &cache, format),
            }
        }
        Command::Status => commands::cmd_status(&dir, format),
        Command::Export { output, exact } => commands::cmd_export(&dir, output.as_deref(), *exact),
        Command::Add {
            url,
            path,
            kind,
            version,
        } => {
            let manifest_path = match &cli.manifest {
                Some(p) => p.clone(),
                None => ir_core::discover_manifest(&dir).map_err(anyhow::Error::from)?,
            };
            commands::cmd_add(
                &manifest_path,
                url,
                path.as_deref(),
                *kind,
                version.as_deref(),
            )
        }
        Command::Prune => commands::cmd_prune(&dir),
        Command::Cache { action } => {
            let cache = ir_core::Cache::new().map_err(anyhow::Error::from)?;
            match action {
                CacheAction::Clean => commands::cmd_cache_clean(&cache),
                CacheAction::Gc => commands::cmd_cache_gc(&cache, &dir),
            }
        }
        Command::Completions { shell } => {
            clap_complete::generate(*shell, &mut Cli::command(), "ir", &mut std::io::stdout());
            Ok(())
        }
    }
}
