//! Analysis engine: ties git history together with the language plugins to
//! produce the file-level and (optionally) function-level change leaderboards.

use anyhow::{Context, Result};
use clap::ValueEnum;
use git2::Repository;
use globset::GlobSet;
use std::collections::{HashMap, HashSet};

use crate::analyzer::{AnalyzerRegistry, Symbol};
use crate::git_history::{self, HistoryOptions};

/// How a changed line is attributed to nested functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Attribution {
    /// Attribute a change only to the single deepest enclosing function, so
    /// parent and child counts never overlap.
    Innermost,
    /// Attribute a change to every enclosing function; nested changes roll up
    /// into all ancestors.
    Inclusive,
}

/// Everything the engine needs to run one analysis.
pub struct AnalyzeConfig {
    /// Path to start repository discovery from.
    pub repo: String,
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// `None` includes every file; otherwise only paths matching the set.
    pub globset: Option<GlobSet>,
    /// Paths matching this set are dropped (applied after `globset`).
    pub exclude: Option<GlobSet>,
    /// Whether to dig down to function/method granularity.
    pub dig: bool,
    /// Largest function-nesting depth to report (`None` = unlimited).
    pub max_depth: Option<u32>,
    /// How nested changes are attributed to functions.
    pub attribution: Attribution,
    /// Keep only the top N rows of each leaderboard (`None` = keep all).
    pub top: Option<usize>,
}

/// How many commits changed a given file.
pub struct FileStat {
    pub path: String,
    pub changes: u32,
}

/// How many commits changed a given function/method within a file.
pub struct SymbolStat {
    pub path: String,
    pub symbol: String,
    pub changes: u32,
    /// Line range of the symbol as of the most recent commit that touched it.
    pub start_line: u32,
    pub end_line: u32,
    /// Function-nesting depth (see [`crate::analyzer::Symbol::depth`]).
    pub depth: u32,
}

/// Per-symbol accumulator while walking history.
struct SymbolAgg {
    changes: u32,
    /// Commit time of the latest observation, so line numbers track the newest.
    latest_time: i64,
    start_line: u32,
    end_line: u32,
    depth: u32,
}

/// The outcome of an analysis run.
pub struct AnalysisResult {
    pub commit_count: usize,
    /// Earliest and latest commit time (Unix seconds) actually analyzed, if any.
    pub period: Option<(i64, i64)>,
    /// Whether a glob filter was applied (used to explain empty results).
    pub glob_filtered: bool,
    pub files: Vec<FileStat>,
    pub symbols: Vec<SymbolStat>,
}

pub fn analyze(config: &AnalyzeConfig) -> Result<AnalysisResult> {
    // Search upward from `repo` so hotcarpet works from any subdirectory.
    let repo = Repository::discover(&config.repo)
        .with_context(|| format!("no git repository found at or above '{}'", config.repo))?;

    let history = git_history::collect_history(
        &repo,
        &HistoryOptions {
            since: config.since,
            until: config.until,
        },
    )?;

    let registry = AnalyzerRegistry::with_builtins();

    let mut file_counts: HashMap<String, u32> = HashMap::new();
    let mut symbol_counts: HashMap<(String, String), SymbolAgg> = HashMap::new();

    for commit in &history {
        for change in &commit.files {
            if !path_included(config, &change.path) {
                continue;
            }
            *file_counts.entry(change.path.clone()).or_default() += 1;

            if config.dig {
                count_symbols(
                    &repo,
                    &registry,
                    commit,
                    change,
                    config.max_depth,
                    config.attribution,
                    &mut symbol_counts,
                );
            }
        }
    }

    let period = history
        .iter()
        .map(|c| c.time)
        .fold(None, |acc: Option<(i64, i64)>, t| {
            Some(acc.map_or((t, t), |(min, max)| (min.min(t), max.max(t))))
        });

    Ok(AnalysisResult {
        commit_count: history.len(),
        period,
        glob_filtered: config.globset.is_some() || config.exclude.is_some(),
        files: rank_files(file_counts, config.top),
        symbols: rank_symbols(symbol_counts, config.top),
    })
}

/// For one changed file in one commit, attribute the change to each function /
/// method whose line range overlaps the changed lines.
fn count_symbols(
    repo: &Repository,
    registry: &AnalyzerRegistry,
    commit: &git_history::CommitChange,
    change: &git_history::FileChange,
    max_depth: Option<u32>,
    attribution: Attribution,
    symbol_counts: &mut HashMap<(String, String), SymbolAgg>,
) {
    let Some(analyzer) = registry.for_path(&change.path) else {
        return;
    };
    let Some(content) = git_history::read_file_at(repo, &commit.id, &change.path) else {
        return;
    };
    let Ok(symbols) = analyzer.extract_symbols(&change.path, &content) else {
        return;
    };

    let changed: HashSet<u32> = change.changed_lines.iter().copied().collect();
    // A symbol counts at most once per commit, even if many of its lines changed.
    // Keep the line range observed so it can track the newest commit later.
    let mut touched: HashMap<String, (u32, u32, u32)> = HashMap::new();
    let within_depth = |s: &Symbol| max_depth.is_none_or(|max| s.depth <= max);
    let mut mark = |s: &Symbol| {
        touched
            .entry(s.name.clone())
            .or_insert((s.start_line, s.end_line, s.depth));
    };
    match attribution {
        // Every enclosing function within the depth limit gets credit.
        Attribution::Inclusive => {
            for symbol in symbols.iter().filter(|s| within_depth(s)) {
                if changed.iter().any(|&line| symbol.covers(line)) {
                    mark(symbol);
                }
            }
        }
        // Only the single deepest enclosing function gets credit per line.
        Attribution::Innermost => {
            for &line in &changed {
                if let Some(symbol) = innermost_covering(&symbols, line, max_depth) {
                    mark(symbol);
                }
            }
        }
    }
    for (name, (start_line, end_line, depth)) in touched {
        let agg = symbol_counts
            .entry((change.path.clone(), name))
            .or_insert(SymbolAgg {
                changes: 0,
                latest_time: i64::MIN,
                start_line,
                end_line,
                depth,
            });
        agg.changes += 1;
        // Prefer line numbers/depth from the most recent commit that touched it.
        if commit.time >= agg.latest_time {
            agg.latest_time = commit.time;
            agg.start_line = start_line;
            agg.end_line = end_line;
            agg.depth = depth;
        }
    }
}

/// The deepest symbol whose span covers `line` (within `max_depth`). Ties on
/// depth are broken by the smaller span, i.e. the more specific function.
fn innermost_covering(symbols: &[Symbol], line: u32, max_depth: Option<u32>) -> Option<&Symbol> {
    symbols
        .iter()
        .filter(|s| max_depth.is_none_or(|max| s.depth <= max) && s.covers(line))
        .max_by(|a, b| {
            a.depth.cmp(&b.depth).then_with(|| {
                let (span_a, span_b) = (a.end_line - a.start_line, b.end_line - b.start_line);
                span_b.cmp(&span_a)
            })
        })
}

fn path_included(config: &AnalyzeConfig, path: &str) -> bool {
    let included = config.globset.as_ref().is_none_or(|set| set.is_match(path));
    let excluded = config
        .exclude
        .as_ref()
        .is_some_and(|set| set.is_match(path));
    included && !excluded
}

fn rank_files(counts: HashMap<String, u32>, top: Option<usize>) -> Vec<FileStat> {
    let mut files: Vec<FileStat> = counts
        .into_iter()
        .map(|(path, changes)| FileStat { path, changes })
        .collect();
    // Most-changed first; break ties by path for stable output.
    files.sort_by(|a, b| b.changes.cmp(&a.changes).then_with(|| a.path.cmp(&b.path)));
    truncate(files, top)
}

fn rank_symbols(
    counts: HashMap<(String, String), SymbolAgg>,
    top: Option<usize>,
) -> Vec<SymbolStat> {
    let mut symbols: Vec<SymbolStat> = counts
        .into_iter()
        .map(|((path, symbol), agg)| SymbolStat {
            path,
            symbol,
            changes: agg.changes,
            start_line: agg.start_line,
            end_line: agg.end_line,
            depth: agg.depth,
        })
        .collect();
    symbols.sort_by(|a, b| {
        b.changes
            .cmp(&a.changes)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    truncate(symbols, top)
}

fn truncate<T>(mut items: Vec<T>, top: Option<usize>) -> Vec<T> {
    if let Some(n) = top {
        items.truncate(n);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, start: u32, end: u32, depth: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            start_line: start,
            end_line: end,
            depth,
        }
    }

    #[test]
    fn innermost_prefers_deepest_then_smallest() {
        let symbols = vec![
            sym("outer", 1, 100, 1),
            sym("inner", 10, 50, 2),
            sym("deepest", 20, 30, 3),
        ];
        let name = |line, max| innermost_covering(&symbols, line, max).map(|s| s.name.as_str());

        assert_eq!(name(25, None), Some("deepest"));
        assert_eq!(name(40, None), Some("inner")); // inside inner, below deepest
        assert_eq!(name(5, None), Some("outer")); // only outer covers it
        assert_eq!(name(200, None), None); // outside every symbol

        // max_depth clamps attribution to the deepest *kept* ancestor.
        assert_eq!(name(25, Some(2)), Some("inner"));
        assert_eq!(name(25, Some(1)), Some("outer"));
    }
}
