//! Git history extraction via libgit2.
//!
//! Provides the per-commit primitives the engine drives (in parallel): the list
//! of commit ids reachable from `HEAD`, the files a commit changed (cheap,
//! OID-level), the added/modified lines for a chosen subset of files, and a
//! blob reader for a resolved tree.

use anyhow::{Context, Result};
use git2::{Commit, Diff, DiffOptions, Oid, Repository, Sort, Tree};
use std::collections::HashMap;
use std::path::Path;

/// Commit ids reachable from `HEAD`, newest first. The walk can be bounded below
/// two ways:
///
/// - `stop_at`: include that commit (and everything between it and `HEAD`) while
///   excluding its ancestors. The bound is applied topologically — by hiding
///   `stop_at`'s parents — so it is correct regardless of commit timestamps
///   (unlike stopping when the commit happens to be visited in time order).
///   `stop_at` must be `HEAD` or an ancestor of it; callers are expected to have
///   validated reachability already.
/// - `limit`: keep at most this many commits (the N most recent walked).
///
/// Callers decide precedence and pass only the bound that should apply.
pub fn commit_oids(
    repo: &Repository,
    stop_at: Option<Oid>,
    limit: Option<usize>,
) -> Result<Vec<Oid>> {
    let mut revwalk = repo.revwalk()?;
    revwalk
        .push_head()
        .context("repository has no HEAD commit")?;
    revwalk.set_sorting(Sort::TIME)?;

    if let Some(stop_at) = stop_at {
        // Hiding the parents keeps `stop_at` itself (a commit is never its own
        // ancestor) while pruning everything older. A root commit has no
        // parents, so nothing is hidden and the whole history is walked.
        for parent in repo.find_commit(stop_at)?.parent_ids() {
            revwalk.hide(parent)?;
        }
    }

    let mut oids = Vec::new();
    for oid in revwalk {
        oids.push(oid?);
        if limit.is_some_and(|limit| oids.len() >= limit) {
            break;
        }
    }
    Ok(oids)
}

/// Paths changed by `commit` versus its first parent. This is the cheap,
/// OID-level delta (no content is diffed), so it counts any change — including
/// deletions. Renames appear as an add + a delete (rename detection is off).
pub fn changed_paths(repo: &Repository, commit: &Commit) -> Result<Vec<String>> {
    let diff = diff_against_parent(repo, commit, None)?;
    let mut paths = Vec::new();
    for delta in diff.deltas() {
        // The new side carries the path for additions/modifications; fall back
        // to the old side for deletions.
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .and_then(|p| p.to_str());
        if let Some(path) = path {
            paths.push(path.to_string());
        }
    }
    Ok(paths)
}

/// Added/modified line numbers (new side, 1-based) for each of `paths`. A
/// pathspec restricts the diff so only those files are content-diffed, which is
/// far cheaper than diffing the whole commit when the set is small.
pub fn changed_lines(
    repo: &Repository,
    commit: &Commit,
    paths: &[String],
) -> Result<HashMap<String, Vec<u32>>> {
    let mut opts = DiffOptions::new();
    opts.context_lines(0);
    for path in paths {
        // A pathspec is fnmatch-style, so escape glob metacharacters to keep
        // real filenames (e.g. the brackets in Next.js `[id].tsx`) literal.
        opts.pathspec(escape_pathspec(path));
    }

    let diff = diff_against_parent(repo, commit, Some(opts))?;
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
    Ok(per_file)
}

/// Backslash-escape the fnmatch metacharacters in `path` so a pathspec matches
/// it literally (filenames may legitimately contain `*`, `?`, `[`, `]`).
fn escape_pathspec(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        if matches!(ch, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Read the UTF-8 contents of `path` from an already-resolved `tree`. Returns
/// `None` if the path is absent, binary, or not valid UTF-8.
pub fn read_blob_from_tree(repo: &Repository, tree: &Tree, path: &str) -> Option<String> {
    let entry = tree.get_path(Path::new(path)).ok()?;
    let object = entry.to_object(repo).ok()?;
    let blob = object.as_blob()?;
    std::str::from_utf8(blob.content()).ok().map(str::to_string)
}

/// Diff `commit` against its first parent (or the empty tree for a root commit).
/// The returned `Diff` borrows only `repo`, so the trees may be dropped here.
fn diff_against_parent<'a>(
    repo: &'a Repository,
    commit: &Commit,
    opts: Option<DiffOptions>,
) -> Result<Diff<'a>> {
    let tree = commit.tree()?;
    let parent_tree = match commit.parent_count() {
        0 => None,
        _ => Some(commit.parent(0)?.tree()?),
    };
    let mut opts = opts;
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), opts.as_mut())?;
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::escape_pathspec;

    #[test]
    fn escapes_fnmatch_metacharacters() {
        assert_eq!(escape_pathspec("src/app.ts"), "src/app.ts");
        assert_eq!(escape_pathspec("pages/[id].tsx"), "pages/\\[id\\].tsx");
        assert_eq!(escape_pathspec("a?b*c"), "a\\?b\\*c");
    }
}
