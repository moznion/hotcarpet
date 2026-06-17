//! Analysis engine: ties git history together with the language plugins to
//! produce the file-level and (optionally) function-level change leaderboards.

use anyhow::{Context, Result};
use clap::ValueEnum;
use git2::{Oid, Repository, Tree};
use globset::GlobSet;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

use crate::analyzer::{AnalyzerRegistry, Symbol};
use crate::git_history;

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
    /// Stop walking history once this commit is reached (inclusive). Anything
    /// `git rev-parse` accepts; takes precedence over `since`/`until`.
    pub since_commit: Option<String>,
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
    /// Time and revwalk index of the latest observation, so line numbers track
    /// the newest commit deterministically regardless of merge order.
    latest_time: i64,
    latest_seq: usize,
    start_line: u32,
    end_line: u32,
    depth: u32,
}

/// What one commit contributes, computed independently on a worker thread.
struct CommitOutcome {
    time: i64,
    /// Position in the revwalk (0 = newest); used to break `latest_time` ties.
    seq: usize,
    /// Paths (matching the filters) the commit changed; each is one file hit.
    files: Vec<String>,
    /// Function/method hits, already de-duplicated within this commit.
    symbols: Vec<SymbolHit>,
}

/// A single function/method credited with a change in one commit.
struct SymbolHit {
    path: String,
    name: String,
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

pub fn analyze(config: &AnalyzeConfig, registry: &AnalyzerRegistry) -> Result<AnalysisResult> {
    // Search upward from `repo` so hotcarpet works from any subdirectory.
    let repo = Repository::discover(&config.repo)
        .with_context(|| format!("no git repository found at or above '{}'", config.repo))?;
    // libgit2 handles are not shareable across threads, so each worker reopens
    // the repository (cheap) and operates on its own handle.
    let git_dir = repo.path().to_path_buf();

    // Resolve the optional history floor up front so a bad or out-of-range
    // revision fails fast, before any diffing work begins.
    let stop_at = config
        .since_commit
        .as_deref()
        .map(|rev| resolve_floor_commit(&repo, rev))
        .transpose()?;
    let oids = git_history::commit_oids(&repo, stop_at)?;

    // Each commit is independent: diff, read blobs, and parse in parallel.
    let outcomes: Vec<Option<CommitOutcome>> = oids
        .par_iter()
        .enumerate()
        .map_init(
            || {
                Repository::open(&git_dir)
                    .expect("failed to reopen git repository in worker thread")
            },
            |repo, (seq, &oid)| analyze_commit(repo, config, registry, seq, oid),
        )
        .collect::<Result<Vec<_>>>()?;

    // Merge the per-commit results sequentially (cheap compared to the work above).
    let mut commit_count = 0usize;
    let mut period: Option<(i64, i64)> = None;
    let mut file_counts: HashMap<String, u32> = HashMap::new();
    let mut symbol_counts: HashMap<(String, String), SymbolAgg> = HashMap::new();

    for outcome in outcomes.into_iter().flatten() {
        commit_count += 1;
        period = Some(period.map_or((outcome.time, outcome.time), |(min, max)| {
            (min.min(outcome.time), max.max(outcome.time))
        }));
        for path in outcome.files {
            *file_counts.entry(path).or_default() += 1;
        }
        for hit in outcome.symbols {
            let agg = symbol_counts
                .entry((hit.path, hit.name))
                .or_insert(SymbolAgg {
                    changes: 0,
                    latest_time: i64::MIN,
                    latest_seq: usize::MAX,
                    start_line: hit.start_line,
                    end_line: hit.end_line,
                    depth: hit.depth,
                });
            agg.changes += 1;
            // Track line numbers/depth from the most recent commit (highest time;
            // ties broken by the smaller revwalk index, i.e. the newer commit).
            let more_recent = outcome.time > agg.latest_time
                || (outcome.time == agg.latest_time && outcome.seq < agg.latest_seq);
            if more_recent {
                agg.latest_time = outcome.time;
                agg.latest_seq = outcome.seq;
                agg.start_line = hit.start_line;
                agg.end_line = hit.end_line;
                agg.depth = hit.depth;
            }
        }
    }

    Ok(AnalysisResult {
        commit_count,
        period,
        glob_filtered: config.globset.is_some() || config.exclude.is_some(),
        files: rank_files(file_counts, config.top),
        symbols: rank_symbols(symbol_counts, config.top),
    })
}

/// Compute one commit's contribution. Returns `None` for commits outside the
/// time window or that changed no files (mirroring the old skip behavior).
fn analyze_commit(
    repo: &Repository,
    config: &AnalyzeConfig,
    registry: &AnalyzerRegistry,
    seq: usize,
    oid: Oid,
) -> Result<Option<CommitOutcome>> {
    let commit = repo.find_commit(oid)?;
    let time = commit.time().seconds();
    if config.since.is_some_and(|since| time < since) {
        return Ok(None);
    }
    if config.until.is_some_and(|until| time > until) {
        return Ok(None);
    }

    // Cheap, OID-level pass: which files changed (no content diffed yet).
    let changed = git_history::changed_paths(repo, &commit)?;
    if changed.is_empty() {
        return Ok(None);
    }

    // File-level leaderboard counts every changed path that passes the filters.
    // The dig pass only needs files an analyzer can handle.
    let mut files = Vec::new();
    let mut analyzable = Vec::new();
    for path in changed {
        if !path_included(config, &path) {
            continue;
        }
        if config.dig && registry.for_path(&path).is_some() {
            analyzable.push(path.clone());
        }
        files.push(path);
    }

    let mut symbols = Vec::new();
    if !analyzable.is_empty() {
        // Content-diff only the analyzable files to get their changed lines.
        let lines = git_history::changed_lines(repo, &commit, &analyzable)?;
        let tree = commit.tree()?;
        for (path, changed_lines) in &lines {
            collect_hits(
                repo,
                &tree,
                registry,
                path,
                changed_lines,
                config,
                &mut symbols,
            );
        }
    }

    Ok(Some(CommitOutcome {
        time,
        seq,
        files,
        symbols,
    }))
}

/// Attribute one changed file's lines to the function(s) that own them,
/// appending the (per-commit de-duplicated) hits to `out`.
fn collect_hits(
    repo: &Repository,
    tree: &Tree,
    registry: &AnalyzerRegistry,
    path: &str,
    changed_lines: &[u32],
    config: &AnalyzeConfig,
    out: &mut Vec<SymbolHit>,
) {
    let Some(analyzer) = registry.for_path(path) else {
        return;
    };
    let Some(content) = git_history::read_blob_from_tree(repo, tree, path) else {
        return;
    };
    let Ok(symbols) = analyzer.extract_symbols(path, &content) else {
        return;
    };

    let changed: HashSet<u32> = changed_lines.iter().copied().collect();
    let max_depth = config.max_depth;
    // A symbol counts at most once per commit, even if many of its lines changed.
    let mut touched: HashMap<String, (u32, u32, u32)> = HashMap::new();
    let within_depth = |s: &Symbol| max_depth.is_none_or(|max| s.depth <= max);
    match config.attribution {
        // Every enclosing function within the depth limit gets credit.
        Attribution::Inclusive => {
            for symbol in symbols.iter().filter(|s| within_depth(s)) {
                if changed.iter().any(|&line| symbol.covers(line)) {
                    touched.entry(symbol.name.clone()).or_insert((
                        symbol.start_line,
                        symbol.end_line,
                        symbol.depth,
                    ));
                }
            }
        }
        // Only the single deepest enclosing function gets credit per line.
        // Iterate lines in sorted order so that, when distinct same-named
        // functions collide, the kept range is deterministic (not HashSet order).
        Attribution::Innermost => {
            let mut lines: Vec<u32> = changed.iter().copied().collect();
            lines.sort_unstable();
            for line in lines {
                if let Some(symbol) = innermost_covering(&symbols, line, max_depth) {
                    touched.entry(symbol.name.clone()).or_insert((
                        symbol.start_line,
                        symbol.end_line,
                        symbol.depth,
                    ));
                }
            }
        }
    }

    for (name, (start_line, end_line, depth)) in touched {
        out.push(SymbolHit {
            path: path.to_string(),
            name,
            start_line,
            end_line,
            depth,
        });
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

/// Resolve a user-supplied revision (hash, abbreviated hash, ref, ...) to the id
/// of the commit it points at, requiring it to be `HEAD` or an ancestor of it.
/// A commit off to the side of HEAD's history can never bound the walk, so we
/// reject it rather than silently scan the entire history.
fn resolve_floor_commit(repo: &Repository, rev: &str) -> Result<Oid> {
    let stop_at = repo
        .revparse_single(rev)
        .with_context(|| format!("could not resolve commit '{rev}'"))?
        .peel_to_commit()
        .map(|commit| commit.id())
        .with_context(|| format!("'{rev}' does not refer to a commit"))?;

    let head = repo
        .head()?
        .peel_to_commit()
        .context("repository has no HEAD commit")?
        .id();
    if head != stop_at && !repo.graph_descendant_of(head, stop_at)? {
        anyhow::bail!("commit '{rev}' is not reachable from HEAD");
    }
    Ok(stop_at)
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
