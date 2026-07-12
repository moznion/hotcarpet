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
    /// `git rev-parse` accepts; takes precedence over `since`/`until` and
    /// `max_commits`.
    pub since_commit: Option<String>,
    /// Walk back at most this many commits from `HEAD` (`None` = no limit).
    /// Takes precedence over `since`/`until`, but is overridden by `since_commit`.
    pub max_commits: Option<usize>,
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
    /// Paths whose analyzer failed to parse this commit's revision, so dig-down
    /// fell back to file-level counting for them.
    parse_failures: Vec<String>,
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
    /// Total distinct files that changed and passed the filters, before any
    /// `--top` truncation of the leaderboard.
    pub total_files: usize,
    /// Distinct files an analyzer failed to parse at some revision (dig-down fell
    /// back to file-level counting for them), sorted by path. Empty when dig-down
    /// is disabled or every file parsed cleanly.
    pub parse_failures: Vec<String>,
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
    // `--since-commit` wins over `--max-commits`, so the count limit only
    // applies when no floor commit was given.
    let limit = if stop_at.is_some() {
        None
    } else {
        config.max_commits
    };
    let oids = git_history::commit_oids(&repo, stop_at, limit)?;

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

    let glob_filtered = config.globset.is_some() || config.exclude.is_some();
    Ok(reduce_outcomes(outcomes, config.top, glob_filtered))
}

/// Merge the independent per-commit results into the final leaderboards. Split
/// out from [`analyze`] so the aggregation (change counts, `total_files`, the
/// de-duplicated/sorted parse-failure list, and "latest commit wins" line
/// tracking) can be unit-tested without a git repository. `None` outcomes are
/// commits that were skipped and contribute nothing.
fn reduce_outcomes(
    outcomes: Vec<Option<CommitOutcome>>,
    top: Option<usize>,
    glob_filtered: bool,
) -> AnalysisResult {
    let mut commit_count = 0usize;
    let mut period: Option<(i64, i64)> = None;
    let mut file_counts: HashMap<String, u32> = HashMap::new();
    let mut symbol_counts: HashMap<(String, String), SymbolAgg> = HashMap::new();
    let mut parse_failures: HashSet<String> = HashSet::new();

    for outcome in outcomes.into_iter().flatten() {
        commit_count += 1;
        period = Some(period.map_or((outcome.time, outcome.time), |(min, max)| {
            (min.min(outcome.time), max.max(outcome.time))
        }));
        for path in outcome.files {
            *file_counts.entry(path).or_default() += 1;
        }
        parse_failures.extend(outcome.parse_failures);
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

    let total_files = file_counts.len();
    let mut parse_failures: Vec<String> = parse_failures.into_iter().collect();
    parse_failures.sort();

    AnalysisResult {
        commit_count,
        period,
        glob_filtered,
        total_files,
        parse_failures,
        files: rank_files(file_counts, top),
        symbols: rank_symbols(symbol_counts, top),
    }
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

    let mut hits = CommitHits::default();
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
                &mut hits,
            );
        }
    }

    Ok(Some(CommitOutcome {
        time,
        seq,
        files,
        symbols: hits.symbols,
        parse_failures: hits.parse_failures,
    }))
}

/// What one commit's dig-down pass produces, filled in by [`collect_hits`].
#[derive(Default)]
struct CommitHits {
    /// Function/method hits, de-duplicated within the commit.
    symbols: Vec<SymbolHit>,
    /// Paths whose analyzer failed to parse this revision.
    parse_failures: Vec<String>,
}

/// Attribute one changed file's lines to the function(s) that own them,
/// appending the (per-commit de-duplicated) hits to `hits.symbols`. A path whose
/// analyzer fails to parse this revision is recorded in `hits.parse_failures`.
fn collect_hits(
    repo: &Repository,
    tree: &Tree,
    registry: &AnalyzerRegistry,
    path: &str,
    changed_lines: &[u32],
    config: &AnalyzeConfig,
    hits: &mut CommitHits,
) {
    let Some(analyzer) = registry.for_path(path) else {
        return;
    };
    let Some(content) = git_history::read_blob_from_tree(repo, tree, path) else {
        return;
    };
    // A parse failure is real feedback: the file changed and an analyzer claims
    // its extension, but its contents at this revision could not be parsed, so
    // its changes are only counted at the file level.
    let symbols = match analyzer.extract_symbols(path, &content) {
        Ok(symbols) => symbols,
        Err(_) => {
            hits.parse_failures.push(path.to_string());
            return;
        }
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
        hits.symbols.push(SymbolHit {
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

    fn hit(path: &str, name: &str, start: u32, end: u32, depth: u32) -> SymbolHit {
        SymbolHit {
            path: path.to_string(),
            name: name.to_string(),
            start_line: start,
            end_line: end,
            depth,
        }
    }

    fn outcome(
        time: i64,
        seq: usize,
        files: &[&str],
        symbols: Vec<SymbolHit>,
        parse_failures: &[&str],
    ) -> Option<CommitOutcome> {
        Some(CommitOutcome {
            time,
            seq,
            files: files.iter().map(|s| s.to_string()).collect(),
            symbols,
            parse_failures: parse_failures.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn reduce_counts_files_and_total() {
        let outcomes = vec![
            outcome(10, 0, &["a.go", "b.go"], vec![], &[]),
            outcome(20, 1, &["a.go", "README.md"], vec![], &[]),
        ];
        let result = reduce_outcomes(outcomes, None, false);
        assert_eq!(result.commit_count, 2);
        // Three distinct files touched across the two commits.
        assert_eq!(result.total_files, 3);
        // `a.go` changed in both commits and leads the leaderboard.
        assert_eq!(result.files[0].path, "a.go");
        assert_eq!(result.files[0].changes, 2);
    }

    #[test]
    fn reduce_dedups_and_sorts_parse_failures() {
        // The same file fails to parse in two commits, plus another file once.
        let outcomes = vec![
            outcome(10, 0, &["z.go", "a.ts"], vec![], &["z.go"]),
            outcome(20, 1, &["z.go"], vec![], &["z.go", "a.ts"]),
        ];
        let result = reduce_outcomes(outcomes, None, false);
        // Distinct failing paths, sorted.
        assert_eq!(result.parse_failures, vec!["a.ts", "z.go"]);
    }

    #[test]
    fn reduce_has_no_parse_failures_when_all_parse() {
        let outcomes = vec![outcome(
            10,
            0,
            &["a.go"],
            vec![hit("a.go", "f", 1, 3, 1)],
            &[],
        )];
        let result = reduce_outcomes(outcomes, None, false);
        assert!(result.parse_failures.is_empty());
    }

    #[test]
    fn reduce_total_files_ignores_top_truncation() {
        let outcomes = vec![outcome(10, 0, &["a", "b", "c"], vec![], &[])];
        let result = reduce_outcomes(outcomes, Some(1), false);
        // The leaderboard is capped at 1 row, but total_files still counts all 3.
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.total_files, 3);
    }

    #[test]
    fn reduce_symbol_line_range_tracks_latest_commit() {
        // The same symbol is credited in two commits at different line ranges;
        // the newer commit (higher time) wins the reported range.
        let older = outcome(10, 1, &["a.go"], vec![hit("a.go", "f", 5, 9, 1)], &[]);
        let newer = outcome(20, 0, &["a.go"], vec![hit("a.go", "f", 1, 4, 1)], &[]);
        // Feed them out of chronological order to prove ordering is by time.
        let result = reduce_outcomes(vec![older, newer], None, false);
        let f = result.symbols.iter().find(|s| s.symbol == "f").unwrap();
        assert_eq!(f.changes, 2);
        assert_eq!((f.start_line, f.end_line), (1, 4));
    }

    #[test]
    fn reduce_skips_none_outcomes() {
        let outcomes = vec![None, outcome(10, 0, &["a.go"], vec![], &[]), None];
        let result = reduce_outcomes(outcomes, None, false);
        assert_eq!(result.commit_count, 1);
        assert_eq!(result.total_files, 1);
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

    // --- End-to-end tests over a real (temporary) git repository. -----------
    //
    // These drive the whole pipeline — history walk, per-commit diff, blob read,
    // language parse, and merge — so the parse-failure reporting is exercised the
    // way a real run hits it, not just the isolated `reduce_outcomes` unit.

    use git2::{Signature, Time};
    use std::path::{Path, PathBuf};

    /// A throwaway git repository under the system temp dir. Each commit writes
    /// the given files into the work tree, stages them, and commits against the
    /// previous tip with a deterministic, strictly increasing timestamp. The
    /// directory is removed on drop.
    struct TempRepo {
        dir: PathBuf,
        repo: Repository,
        seq: i64,
    }

    impl TempRepo {
        fn new(name: &str) -> Self {
            // Unique per (process, test) so parallel tests don't collide; cleaned
            // first in case a previous run left it behind.
            let dir =
                std::env::temp_dir().join(format!("hotcarpet-it-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let repo = Repository::init(&dir).unwrap();
            Self { dir, repo, seq: 0 }
        }

        fn commit(&mut self, message: &str, files: &[(&str, &str)]) {
            for (name, content) in files {
                std::fs::write(self.dir.join(name), content).unwrap();
            }
            let mut index = self.repo.index().unwrap();
            for (name, _) in files {
                index.add_path(Path::new(name)).unwrap();
            }
            index.write().unwrap();
            let tree = self.repo.find_tree(index.write_tree().unwrap()).unwrap();

            // Fixed base time plus the commit sequence keeps history ordering
            // deterministic without depending on the wall clock.
            let sig = Signature::new(
                "Tester",
                "tester@example.com",
                &Time::new(1_600_000_000 + self.seq * 100, 0),
            )
            .unwrap();
            self.seq += 1;

            let parent = self.repo.head().ok().and_then(|h| h.peel_to_commit().ok());
            let parents: Vec<&git2::Commit> = parent.as_ref().into_iter().collect();
            self.repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
                .unwrap();
        }

        fn path(&self) -> String {
            self.dir.to_str().unwrap().to_string()
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn default_config(repo: String) -> AnalyzeConfig {
        AnalyzeConfig {
            repo,
            since: None,
            until: None,
            since_commit: None,
            max_commits: None,
            globset: None,
            exclude: None,
            dig: true,
            max_depth: None,
            attribution: Attribution::Innermost,
            top: None,
        }
    }

    #[test]
    fn analyze_reports_parse_failure_across_history() {
        let mut repo = TempRepo::new("parse-failure");
        // c1: valid Go plus a non-analyzable file.
        repo.commit(
            "c1",
            &[
                ("a.go", "package main\nfunc a() int { return 1 }\n"),
                ("README.md", "# hi\n"),
            ],
        );
        // c2: a.go is broken at this revision — it must be reported as a failure.
        repo.commit(
            "c2",
            &[("a.go", "package main\nfunc a( int { return 1 }\n")],
        );
        // c3: a.go is fixed again and a new valid file appears.
        repo.commit(
            "c3",
            &[
                ("a.go", "package main\nfunc a() int { return 2 }\n"),
                ("b.go", "package main\nfunc b() {}\n"),
            ],
        );

        let registry = AnalyzerRegistry::with_builtins();
        let result = analyze(&default_config(repo.path()), &registry).unwrap();

        assert_eq!(result.commit_count, 3);
        // a.go, README.md, b.go — README counts but is never parsed.
        assert_eq!(result.total_files, 3);
        // Only a.go failed to parse (at c2), reported once despite three touches.
        assert_eq!(result.parse_failures, vec!["a.go".to_string()]);

        let changes = |path: &str| {
            result
                .files
                .iter()
                .find(|f| f.path == path)
                .map(|f| f.changes)
        };
        assert_eq!(changes("a.go"), Some(3));
        assert_eq!(changes("b.go"), Some(1));

        // Dig-down still credits `a` for the two commits it parsed (c1 and c3),
        // and `b` for its single commit; the broken c2 contributes no symbol.
        let sym_changes = |name: &str| {
            result
                .symbols
                .iter()
                .find(|s| s.symbol == name)
                .map(|s| s.changes)
        };
        assert_eq!(sym_changes("a"), Some(2));
        assert_eq!(sym_changes("b"), Some(1));
    }

    #[test]
    fn analyze_clean_repo_reports_no_parse_failures() {
        let mut repo = TempRepo::new("clean");
        repo.commit("c1", &[("main.go", "package main\nfunc main() {}\n")]);
        repo.commit(
            "c2",
            &[("main.go", "package main\nfunc main() { _ = 1 }\n")],
        );

        let registry = AnalyzerRegistry::with_builtins();
        let result = analyze(&default_config(repo.path()), &registry).unwrap();

        assert_eq!(result.total_files, 1);
        assert!(result.parse_failures.is_empty());
        assert_eq!(
            result
                .symbols
                .iter()
                .find(|s| s.symbol == "main")
                .map(|s| s.changes),
            Some(2),
        );
    }

    #[test]
    fn analyze_without_dig_reports_no_parse_failures() {
        // With dig-down off, no file is parsed, so even a broken revision yields
        // no parse failures — only the file leaderboard is produced.
        let mut repo = TempRepo::new("no-dig");
        repo.commit("c1", &[("a.go", "package main\nfunc a( int {\n")]);

        let registry = AnalyzerRegistry::with_builtins();
        let mut config = default_config(repo.path());
        config.dig = false;
        let result = analyze(&config, &registry).unwrap();

        assert_eq!(result.total_files, 1);
        assert!(result.parse_failures.is_empty());
        assert!(result.symbols.is_empty());
    }
}
