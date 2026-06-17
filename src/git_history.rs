//! Git history extraction via libgit2.
//!
//! Walks the commit graph from `HEAD` and, for each commit, diffs it against its
//! first parent to discover which files changed and which lines were added or
//! modified (expressed as line numbers in the *new* version of each file).

use anyhow::{Context, Result};
use git2::{Commit, DiffOptions, Repository, Sort};
use std::collections::HashMap;
use std::path::Path;

/// A file touched by a commit, with the new-side line numbers that changed.
pub struct FileChange {
    pub path: String,
    /// 1-based line numbers in the new version of the file that were added or
    /// modified by this commit.
    pub changed_lines: Vec<u32>,
}

/// One commit and the set of files it changed.
pub struct CommitChange {
    pub id: String,
    /// Commit time in seconds since the Unix epoch.
    pub time: i64,
    pub files: Vec<FileChange>,
}

/// Inclusive time window (Unix seconds) used to filter commits.
#[derive(Default)]
pub struct HistoryOptions {
    pub since: Option<i64>,
    pub until: Option<i64>,
}

/// Walk history from `HEAD`, returning every commit within the time window that
/// changed at least one file.
pub fn collect_history(repo: &Repository, opts: &HistoryOptions) -> Result<Vec<CommitChange>> {
    let mut revwalk = repo.revwalk()?;
    revwalk
        .push_head()
        .context("repository has no HEAD commit")?;
    revwalk.set_sorting(Sort::TIME)?;

    let mut commits = Vec::new();
    for oid in revwalk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let time = commit.time().seconds();

        if opts.since.is_some_and(|since| time < since) {
            continue;
        }
        if opts.until.is_some_and(|until| time > until) {
            continue;
        }

        let files = changed_files(repo, &commit)?;
        if files.is_empty() {
            continue;
        }
        commits.push(CommitChange {
            id: oid.to_string(),
            time,
            files,
        });
    }
    Ok(commits)
}

/// Diff `commit` against its first parent (or the empty tree for a root commit)
/// and collect the added/modified lines per file.
fn changed_files(repo: &Repository, commit: &Commit) -> Result<Vec<FileChange>> {
    let tree = commit.tree()?;
    let parent_tree = match commit.parent_count() {
        0 => None,
        _ => Some(commit.parent(0)?.tree()?),
    };

    let mut diff_opts = DiffOptions::new();
    diff_opts.context_lines(0);
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_opts))?;

    let mut per_file: HashMap<String, Vec<u32>> = HashMap::new();
    diff.foreach(
        &mut |_delta, _progress| true,
        None,
        None,
        Some(&mut |delta, _hunk, line| {
            // '+' marks an added line on the new side of the diff.
            if line.origin() == '+'
                && let (Some(lineno), Some(path)) = (
                    line.new_lineno(),
                    delta.new_file().path().and_then(|p| p.to_str()),
                )
            {
                per_file.entry(path.to_string()).or_default().push(lineno);
            }
            true
        }),
    )?;

    Ok(per_file
        .into_iter()
        .map(|(path, changed_lines)| FileChange {
            path,
            changed_lines,
        })
        .collect())
}

/// Read the UTF-8 contents of `path` as it existed in commit `commit_id`.
/// Returns `None` if the path is absent, binary, or not valid UTF-8.
pub fn read_file_at(repo: &Repository, commit_id: &str, path: &str) -> Option<String> {
    let oid = git2::Oid::from_str(commit_id).ok()?;
    let tree = repo.find_commit(oid).ok()?.tree().ok()?;
    let entry = tree.get_path(Path::new(path)).ok()?;
    let object = entry.to_object(repo).ok()?;
    let blob = object.as_blob()?;
    std::str::from_utf8(blob.content()).ok().map(str::to_string)
}
