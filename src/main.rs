//! `rustify`: plans and tracks the port of a TypeScript codebase to Rust,
//! one dependency-ordered batch at a time.

mod analyze;
mod brief;
mod commands;
mod compare;
mod components;
mod config;
mod e2e;
mod graph;
mod index;
mod mappings;
mod packages;
mod plan;
mod provenance;
mod ratchet;
mod rust_items;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rustify",
    version,
    about = "Plan and track a TypeScript → Rust port"
)]
struct Cli {
    /// Directory to start from (default: the current directory).
    #[arg(long, global = true)]
    repo: Option<PathBuf>,
    /// Path to rustify.toml, relative to --repo (default: search upward from
    /// it). Also read from RUSTIFY_CONFIG.
    #[arg(long, global = true, env = "RUSTIFY_CONFIG")]
    config: Option<PathBuf>,
    /// Print JSON instead of text where supported.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Overall progress: files, lines, and tests ported, by package.
    Status,
    /// The next batches to port, ranked by how much they unblock.
    Next {
        /// How many batches to list.
        #[arg(long, default_value_t = 3)]
        count: usize,
        /// Only batches touching this workspace package (e.g. @acme/app).
        #[arg(long)]
        package: Option<String>,
        /// Print the full brief for the first batch.
        #[arg(long)]
        brief: bool,
        /// Return a wave: batches that can be ported at the same time because
        /// no two write the same Rust file (shared parent `mod` lines are
        /// merged by the lead).
        #[arg(long)]
        independent: bool,
        /// Only catch-up work with the upstream ref: ported modules whose TS
        /// changed since they were ported, and modules upstream added since
        /// the port began. Each is offered once everything it imports is
        /// current.
        #[arg(long)]
        catch_up: bool,
        /// Write each batch's brief to <dir>/batch-<n>.md (the prompt for a
        /// subagent porting that batch).
        #[arg(long, value_name = "DIR")]
        write_briefs: Option<PathBuf>,
    },
    /// Porting brief for specific TS files (repository-relative).
    Brief {
        #[arg(required = true)]
        files: Vec<String>,
    },
    /// Claim files for porting (status in_progress).
    Start {
        #[arg(required = true)]
        files: Vec<String>,
        /// Claim even if a dependency is not ported yet.
        #[arg(long)]
        force: bool,
    },
    /// Record ported files and map their exports onto Rust items.
    Done {
        #[arg(required = true)]
        files: Vec<String>,
        /// TS test files whose cases were ported alongside.
        #[arg(long = "test")]
        tests: Vec<String>,
        /// Ported TS test files outside the current dependency graph. Only
        /// valid when recording one module; these remain tracked by drift.
        #[arg(long = "test-provenance")]
        test_provenance: Vec<String>,
        /// Explicit `tsName=rust::path` mappings for names the scan misses.
        /// `tsName=-` records an export with no Rust counterpart on purpose
        /// (say why in `--notes`).
        #[arg(long = "map")]
        maps: Vec<String>,
        /// Rust file (relative to the crate's src/) when it differs from the
        /// mirrored path. Only valid with a single TS file.
        #[arg(long)]
        rust: Option<String>,
        /// Mark as verified by the binary-level contract suite.
        #[arg(long)]
        verified: bool,
        #[arg(long)]
        notes: Option<String>,
    },
    /// Mark files as not needed in Rust.
    Skip {
        #[arg(required = true)]
        files: Vec<String>,
        #[arg(long, required = true)]
        reason: String,
    },
    /// Mark files as provided by a crate or std instead of a port.
    Replace {
        #[arg(required = true)]
        files: Vec<String>,
        /// What provides the behavior, e.g. `tokio::process`.
        #[arg(long = "with", required = true)]
        with: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Find a symbol across TS exports, the port index, Rust items, and
    /// package mappings.
    Find { query: String },
    /// Validate the index against the graph and the Rust crate.
    Check,
    /// Ported modules whose TS source or ported tests changed upstream since
    /// their recorded `ts_base`: the work list for a fixup pass.
    Drift {
        /// Compare against this ref instead of the configured upstream
        /// (fetch it first).
        #[arg(long)]
        against: Option<String>,
    },
    /// Fill `ts_blobs` on done entries that lack it, from the blobs at their
    /// `ts_base`. Run once to migrate an index; `done` records blobs itself.
    StampBlobs,
    /// CI gate: fail when this branch makes a ported module stale (changes
    /// its TS or recorded tests without porting and re-running `done`) or
    /// adds a ported module importing unported code. Modules already stale
    /// where the branch left `--base` are grandfathered.
    Ratchet {
        /// The pull request's base (the tool compares against its merge base
        /// with HEAD, so base commits made after branching never count).
        /// Defaults to the configured upstream.
        #[arg(long)]
        base: Option<String>,
    },
    /// Dependency graph summary, or `--json` for the full graph.
    Graph,
    /// Which tests run the program as a process without importing its
    /// source. Those can run unchanged against the Rust binary; run this
    /// before porting to see whether such a suite exists.
    E2e,
    /// Run the cases in `compare.toml` through the TypeScript program and
    /// the Rust one, one at a time, and fail when exit code, stdout, stderr,
    /// or the files written differ in a case that is not accepted.
    Compare {
        /// Only run this case (repeatable).
        #[arg(long = "case", value_name = "NAME")]
        cases: Vec<String>,
        /// Write each side's normalized output to <dir>/<case>/ for diffing.
        #[arg(long, value_name = "DIR")]
        keep: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    exit_quietly_on_closed_stdout();
    let cli = Cli::parse();
    let start = match &cli.repo {
        Some(repo) => repo.clone(),
        None => std::env::current_dir()?,
    };
    let ws = config::Workspace::discover(&start, cli.config.as_deref())?;
    let ok = commands::run(&ws, cli.command, cli.json)?;
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// `rustify graph | head` closes stdout early, and `println!` panics on the
/// write error. Exit like other CLIs do instead of printing a panic.
fn exit_quietly_on_closed_stdout() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or_default();
        if message.contains("failed printing to stdout") && message.contains("Broken pipe") {
            std::process::exit(141);
        }
        default(info);
    }));
}
