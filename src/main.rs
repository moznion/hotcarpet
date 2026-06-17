//! hotcarpet — find the "hot" spots of a codebase from its git history.

mod analyzer;
mod cli;
mod config;
mod engine;
mod git_history;
mod output;

use anyhow::Result;
use clap::Parser;

use crate::analyzer::AnalyzerRegistry;
use crate::cli::Cli;
use crate::config::Config;
use crate::engine::{AnalyzeConfig, analyze};
use crate::output::Format;

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(jobs) = cli.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()
            .map_err(|e| anyhow::anyhow!("failed to configure {jobs} worker threads: {e}"))?;
    }

    let user_config = Config::resolve(cli.config.as_deref(), &cli.repo)?;
    let mut registry = AnalyzerRegistry::with_builtins();
    registry.apply_config(&user_config);

    let config = AnalyzeConfig {
        repo: cli.repo.clone(),
        since: cli.since_timestamp()?,
        until: cli.until_timestamp()?,
        since_commit: cli.since_commit.clone(),
        max_commits: cli.max_commits_limit(),
        globset: cli.include_globset()?,
        exclude: cli.exclude_globset()?,
        dig: !cli.no_dig,
        max_depth: cli.max_depth,
        attribution: cli.nested,
        top: cli.top_limit(),
    };

    let format = if cli.table {
        Format::Table
    } else {
        Format::Json
    };

    let result = analyze(&config, &registry)?;
    output::render(&result, format);
    Ok(())
}
