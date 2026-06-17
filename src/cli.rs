//! Command-line interface definition and argument parsing helpers.

use anyhow::{Context, Result};
use chrono::{NaiveDate, NaiveTime};
use clap::Parser;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::engine::Attribution;

/// Analyze git history to surface the most frequently changed files — and,
/// optionally, the most frequently changed functions and methods.
#[derive(Parser, Debug)]
#[command(name = "hotcarpet", version, about)]
pub struct Cli {
    /// Path to the git repository to analyze. The repository is discovered by
    /// searching upward from here, so a subdirectory works too.
    #[arg(short, long, default_value = ".", value_name = "PATH")]
    pub repo: String,

    /// Path to a TOML config file. When omitted, `.hotcarpet.toml` is
    /// discovered by searching upward from `--repo`.
    #[arg(short = 'c', long, value_name = "PATH")]
    pub config: Option<String>,

    /// Only consider commits on or after this date (YYYY-MM-DD).
    #[arg(long, value_name = "DATE")]
    pub since: Option<String>,

    /// Only consider commits on or before this date (YYYY-MM-DD).
    #[arg(long, value_name = "DATE")]
    pub until: Option<String>,

    /// Walk history back no further than this commit. The traversal stops once
    /// it reaches this commit (inclusive). This takes precedence over `--since`
    /// / `--until`: those still filter the commits that are walked, but the walk
    /// always stops here regardless. Accepts anything `git rev-parse` resolves
    /// (full or abbreviated hash, ref, etc.).
    #[arg(long, value_name = "COMMIT")]
    pub since_commit: Option<String>,

    /// Walk back at most this many commits from HEAD (the N most recent). This
    /// takes precedence over `--since` / `--until`: those still filter the
    /// commits that are walked, but the walk never goes back more than N.
    /// `--since-commit` takes precedence over this. Use 0 for no limit.
    #[arg(long, value_name = "N")]
    pub max_commits: Option<usize>,

    /// Globs of files to include. Omit to include every file.
    /// Example: hotcarpet 'src/**/*.ts'
    #[arg(value_name = "GLOB")]
    pub globs: Vec<String>,

    /// Globs of files to exclude; repeatable. Applied after the include globs.
    /// Example: --exclude '**/*.test.ts'
    #[arg(short, long = "exclude", value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Show only the top N rows of each leaderboard. Use 0 for no limit.
    #[arg(short, long, default_value_t = 20)]
    pub top: usize,

    /// Skip the function / method-level dig-down and report only the file
    /// leaderboard. Dig-down is on by default.
    #[arg(long)]
    pub no_dig: bool,

    /// Largest function-nesting depth to report (1 = top-level only). Functions
    /// nested deeper are ignored. Unlimited by default.
    #[arg(long, value_name = "N")]
    pub max_depth: Option<u32>,

    /// How a change inside a nested function is counted: `innermost` credits
    /// only the deepest enclosing function; `inclusive` also rolls it up into
    /// every ancestor.
    #[arg(long = "nested", value_enum, default_value = "innermost")]
    pub nested: Attribution,

    /// Render human-readable tables instead of the default JSON output.
    #[arg(long)]
    pub table: bool,

    /// Number of worker threads. Defaults to the number of logical CPUs.
    #[arg(short, long, value_name = "N")]
    pub jobs: Option<usize>,
}

impl Cli {
    /// Parse `--since` into an inclusive lower-bound Unix timestamp (start of day, UTC).
    pub fn since_timestamp(&self) -> Result<Option<i64>> {
        self.since
            .as_deref()
            .map(|s| parse_date(s).map(|d| d.and_time(NaiveTime::MIN).and_utc().timestamp()))
            .transpose()
    }

    /// Parse `--until` into an inclusive upper-bound Unix timestamp (end of day, UTC).
    pub fn until_timestamp(&self) -> Result<Option<i64>> {
        self.until
            .as_deref()
            .map(|s| {
                let end_of_day = NaiveTime::from_hms_opt(23, 59, 59).unwrap();
                parse_date(s).map(|d| d.and_time(end_of_day).and_utc().timestamp())
            })
            .transpose()
    }

    /// Build the include filter, or `None` when no globs were supplied.
    pub fn include_globset(&self) -> Result<Option<GlobSet>> {
        build_globset(&self.globs)
    }

    /// Build the exclude filter, or `None` when no excludes were supplied.
    pub fn exclude_globset(&self) -> Result<Option<GlobSet>> {
        build_globset(&self.exclude)
    }

    /// `top` as an `Option`, where 0 means "no limit".
    pub fn top_limit(&self) -> Option<usize> {
        (self.top != 0).then_some(self.top)
    }

    /// `max_commits` as a traversal limit, where an absent flag or 0 means
    /// "no limit" (consistent with `--top`).
    pub fn max_commits_limit(&self) -> Option<usize> {
        self.max_commits.filter(|&n| n != 0)
    }
}

fn parse_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("invalid date '{s}', expected YYYY-MM-DD"))
}

/// Build a `GlobSet` from `patterns`, or `None` when the list is empty.
fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        // git paths are repo-root-relative without a `./` prefix, so strip any
        // leading `./` the user (or their shell) supplied.
        let pattern = normalize_glob(pattern);
        // `literal_separator(true)` makes `*` stop at `/`, so patterns behave
        // like familiar shell globs (`**` crosses directories).
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid glob pattern: '{pattern}'"))?;
        builder.add(glob);
    }
    Ok(Some(builder.build()?))
}

/// Strip leading `./` (possibly repeated) so a glob lines up with git's
/// repo-root-relative paths.
fn normalize_glob(pattern: &str) -> &str {
    let mut p = pattern;
    while let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_glob_strips_leading_dot_slash() {
        assert_eq!(normalize_glob("./src/**/*"), "src/**/*");
        assert_eq!(normalize_glob(".//src"), "/src"); // only `./` pairs are peeled
        assert_eq!(normalize_glob("././src"), "src");
        assert_eq!(normalize_glob("src/**/*.ts"), "src/**/*.ts");
    }
}
