# hotcarpet

Find the "hot" spots of a codebase from its git history. hotcarpet ranks the
files — and, optionally, the individual functions and methods — that changed
most often over a given period.

### Examples

```sh
# Hot files and functions across the whole history
hotcarpet

# TypeScript hot functions changed in the first half of 2026
hotcarpet 'src/**/*.ts' --since 2026-01-01 --until 2026-06-30

# Exclude test files from the analysis
hotcarpet 'src/**/*.ts' --exclude '**/*.test.ts'

# Only history back to (and including) a specific commit, e.g. since a release
hotcarpet --since-commit v1.0.0

# Only the 50 most recent commits
hotcarpet --max-commits 50

# Only top-level functions/methods (ignore nested closures)
hotcarpet 'src/**/*.ts' --max-depth 1

# Human-readable tables instead of JSON
hotcarpet --table

# Pipe the JSON into jq — e.g. the 5 hottest functions
hotcarpet 'src/**/*.ts' | jq '.symbols[:5]'

# File-level leaderboard only (faster; no source parsing)
hotcarpet --no-dig
```


## Features

- **File leaderboard** — counts how many commits touched each file (added,
  modified, or deleted it).
- **Time window** — restrict analysis with `--since` / `--until` (defaults to the
  whole history).
- **Commit floor** — stop walking history at a given commit with `--since-commit`
  (inclusive), or cap the walk to the N most recent commits with `--max-commits`.
  Both take precedence over `--since` / `--until`; `--since-commit` wins over
  `--max-commits`.
- **Glob filter** — limit the analysis to matching paths (e.g. `src/**/*.ts`).
- **Dig down** (on by default) — attribute each change to the specific function
  or method it modified, with its line range. Disable with `--no-dig`.
- **Language plugins** — dig-down is powered by pluggable per-language analyzers.
  TypeScript / JavaScript (parsed with [oxc](https://oxc.rs)), Rust (parsed with
  [syn](https://docs.rs/syn)), and Go (parsed with
  [tree-sitter](https://tree-sitter.github.io)) ship in the box. Each can be
  enabled/disabled and have its file extensions configured per language (see
  [Configuration](#configuration)).
- **Parse-failure reporting** — every run reports the total number of analyzed
  files and, when dig-down can't parse a file at some revision, how many files
  failed and which ones (their changes still count toward the file leaderboard,
  just not per-function). In JSON this is the `total_files` / `parse_failures`
  fields; with `--table` it is printed as a warning on stderr, outside the tables.
- **JSON by default** — machine-readable output for piping into `jq` etc.; pass
  `--table` for human-readable tables.

## Usage

```console
$ hotcarpet [OPTIONS] [GLOB]...

Arguments:
  [GLOB]...               Globs of files to include. Omit to include every file

Options:
  -r, --repo <PATH>       Path to the git repository; discovered by searching
                          upward, so a subdirectory works too [default: .]
  -c, --config <PATH>     Path to a TOML config file; otherwise .hotcarpet.toml
                          is discovered by searching upward from --repo
  -e, --exclude <GLOB>    Globs of files to exclude; repeatable
      --since <DATE>      Only commits on or after this date (YYYY-MM-DD)
      --until <DATE>      Only commits on or before this date (YYYY-MM-DD)
      --since-commit <COMMIT>
                          Stop walking history at this commit (inclusive);
                          takes precedence over --since / --until
      --max-commits <N>   Walk back at most N commits from HEAD; takes precedence
                          over --since / --until, but --since-commit wins over it.
                          0 = no limit

  -t, --top <N>          Show top N rows per leaderboard; 0 = no limit [default: 20]
      --no-dig           Skip function/method dig-down; show only the file leaderboard
      --max-depth <N>    Largest function-nesting depth to report (1 = top-level only)
      --nested <MODE>    Nested-change attribution: innermost (default) | inclusive
      --table            Render human-readable tables instead of JSON
  -j, --jobs <N>         Worker threads [default: number of logical CPUs]
```

Commits are analyzed in parallel (diff + parse) across worker threads; results
are deterministic regardless of thread count. Use `-j 1` to run serially.

Dig-down reports nested functions too: a top-level function (or a method of a
top-level class) is **depth 1**, a function defined inside one is depth 2, and so
on — entering a class does not add a level. `--max-depth N` keeps only symbols up
to depth `N`, and each symbol's depth is included in the output.

When a change lands inside a nested function, `--nested` controls who gets the
credit:

- `innermost` (default) — only the deepest enclosing function is credited, so a
  parent and its nested closures never double-count the same change. A parent is
  credited only for changes to its own lines (signature, top-level statements,
  code between closures). Under `--max-depth`, credit is clamped to the deepest
  *kept* ancestor.
- `inclusive` — every enclosing function is credited, so a nested change rolls up
  into all of its ancestors (a big parent function tends to dominate).

Glob patterns match paths **relative to the repository root**, regardless of the
directory you run hotcarpet from — so `hotcarpet 'src/**/*.ts'` always means
`src/` under the repo root. A leading `./` is stripped automatically, and globs
should be quoted so your shell doesn't expand them first.

## How it works

1. `git_history` walks commits from `HEAD`, diffing each against its first parent.
   The file list comes from a cheap OID-level delta (so a file counts as changed
   whenever it is added, modified, **or deleted**); line numbers are computed only
   for the files that need dig-down, via a pathspec-restricted diff.
2. `engine` aggregates per-file change counts. By default it also digs down: for
   each changed file it reads that file's contents at the commit, asks the
   matching language plugin for the function/method spans, and credits the change
   to every symbol whose line range overlaps a changed line (each symbol counts
   at most once per commit). Pass `--no-dig` to skip this.
3. `output` prints the rankings as JSON (default) or as tables (`--table`).
   Diagnostics such as "no file matched the glob" go to stderr, keeping stdout
   valid JSON. A file whose analyzer fails to parse a given revision is recorded
   as a parse failure: it still contributes to the file leaderboard, but not to
   the function leaderboard for that commit. The run reports `total_files` and
   the `parse_failures` list (in JSON), or a stderr warning under `--table`.

## Configuration

Dig-down is configured per language in a `.hotcarpet.toml` file: you can turn an
individual analyzer off, and control which file extensions map to it. The file
is discovered by searching upward from `--repo`, or pass one explicitly with
`-c, --config`.

```toml
# Analyzers are addressed by name (case-insensitive), e.g. [analyzers.typescript].
[analyzers.typescript]
# Replace the analyzer's built-in extension list entirely — e.g. to also dig
# into .vue/.astro files, or to stop digging into .js:
extensions = ["ts", "tsx", "vue", "astro"]

# Turn dig-down off for a language. Its files still count toward the file
# leaderboard; they just aren't parsed into functions/methods.
[analyzers.rust]
enabled = false
```

`enabled` defaults to `true`; set it to `false` to skip a language while still
digging into the others (unlike `--no-dig`, which disables dig-down globally).
`extensions` replaces the analyzer's built-in extension list wholesale; omit it
to keep the built-in list. Extensions are matched case-insensitively and a
leading dot is optional (`"vue"` and `".vue"` are equivalent). A config entry
naming an analyzer that does not exist is reported to stderr and skipped.

## Adding a language plugin

Implement `analyzer::LanguageAnalyzer` (report each function/method's name and
1-based line range from a source string) and register it in
`AnalyzerRegistry::with_builtins`. See `analyzer/typescript.rs` (oxc),
`analyzer/rust.rs` (syn), or `analyzer/golang.rs` (tree-sitter) for reference
implementations. The analyzer's `name()`
doubles as its config key, and its `extensions()` becomes the default mapping
users can override (see
[Configuration](#configuration)).

## Development

```sh
cargo build
cargo test
```
