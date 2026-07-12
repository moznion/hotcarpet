//! Renders analysis results as JSON (default) or human-readable tables.

use std::io::IsTerminal;

use chrono::DateTime;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, Table};
use serde_json::json;

use crate::engine::AnalysisResult;

/// Output format selected on the command line.
#[derive(Clone, Copy)]
pub enum Format {
    Json,
    Table,
}

pub fn render(result: &AnalysisResult, format: Format) {
    match format {
        Format::Json => print_json(result),
        Format::Table => print_table(result),
    }
    // Parse failures go into the JSON payload (above); for tables they are
    // surfaced on stderr so they don't corrupt the rendered tables.
    if matches!(format, Format::Table) && !result.parse_failures.is_empty() {
        warn_parse_failures(result);
    }
    // Always surface the "why is this empty" hint on stderr, regardless of format.
    if result.files.is_empty() {
        warn_empty(result);
    }
}

fn print_json(result: &AnalysisResult) {
    println!(
        "{}",
        serde_json::to_string_pretty(&to_json(result)).unwrap()
    );
}

/// Build the JSON payload for a result. Split out from [`print_json`] so it can
/// be asserted on in tests.
fn to_json(result: &AnalysisResult) -> serde_json::Value {
    let period = result
        .period
        .map(|(from, to)| json!({ "from": format_date(from), "to": format_date(to) }));
    let files: Vec<_> = result
        .files
        .iter()
        .enumerate()
        .map(|(i, s)| json!({ "rank": i + 1, "changes": s.changes, "path": s.path }))
        .collect();
    let symbols: Vec<_> = result
        .symbols
        .iter()
        .enumerate()
        .map(|(i, s)| {
            json!({
                "rank": i + 1,
                "changes": s.changes,
                "symbol": s.symbol,
                "path": s.path,
                "start_line": s.start_line,
                "end_line": s.end_line,
                "depth": s.depth,
            })
        })
        .collect();

    json!({
        "commit_count": result.commit_count,
        "period": period,
        "total_files": result.total_files,
        "parse_failures": {
            "count": result.parse_failures.len(),
            "files": result.parse_failures.clone(),
        },
        "files": files,
        "symbols": symbols,
    })
}

fn print_table(result: &AnalysisResult) {
    match result.period {
        Some((from, to)) => println!(
            "Analyzed {} commit(s) from {} to {}.\n",
            result.commit_count,
            format_date(from),
            format_date(to),
        ),
        None => println!("Analyzed {} commit(s).\n", result.commit_count),
    }

    println!("File change leaderboard");
    if result.files.is_empty() {
        println!("  (no matching changes found)");
    } else {
        println!("{}", file_table(result));
    }

    if !result.symbols.is_empty() {
        println!("\nFunction / method change leaderboard");
        println!("{}", symbol_table(result));
    }
}

fn file_table(result: &AnalysisResult) -> Table {
    let mut table = base_table(["#", "Changes", "File"]);
    for (rank, stat) in result.files.iter().enumerate() {
        table.add_row(vec![
            right(rank + 1),
            right(stat.changes),
            Cell::new(&stat.path),
        ]);
    }
    table
}

fn symbol_table(result: &AnalysisResult) -> Table {
    let mut table = base_table(["#", "Changes", "Function", "Depth", "Lines", "File"]);
    for (rank, stat) in result.symbols.iter().enumerate() {
        table.add_row(vec![
            right(rank + 1),
            right(stat.changes),
            Cell::new(&stat.symbol),
            right(stat.depth),
            right(format!("{}-{}", stat.start_line, stat.end_line)),
            Cell::new(&stat.path),
        ]);
    }
    table
}

fn base_table<const N: usize>(headers: [&str; N]) -> Table {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(headers.iter().map(Cell::new));
    table
}

fn right(value: impl ToString) -> Cell {
    Cell::new(value.to_string()).set_alignment(CellAlignment::Right)
}

/// Emit a warning (to stderr) listing the files dig-down could not parse. Their
/// changes still count toward the file leaderboard, just not per-function.
fn warn_parse_failures(result: &AnalysisResult) {
    warn(&parse_failure_message(result));
}

/// Build the parse-failure warning: a one-line summary followed by one indented
/// line per failing path. Split out from [`warn_parse_failures`] for testing.
fn parse_failure_message(result: &AnalysisResult) -> String {
    let mut msg = format!(
        "warning: failed to parse {} of {} changed file(s); their changes are counted at the \
         file level only:",
        result.parse_failures.len(),
        result.total_files,
    );
    for path in &result.parse_failures {
        msg.push_str(&format!("\n  - {path}"));
    }
    msg
}

/// Emit a hint (to stderr) explaining why the leaderboard came out empty.
fn warn_empty(result: &AnalysisResult) {
    if result.commit_count == 0 {
        warn("warning: no commits matched the selected time range.");
    } else if result.glob_filtered {
        warn(&format!(
            "warning: analyzed {} commit(s) but no changed file matched the given glob(s).\n\
             hint: globs are matched against repo-root-relative paths (e.g. 'src/**/*.ts'); \
             quote them and drop any leading './'.",
            result.commit_count,
        ));
    }
}

/// Print a diagnostic to stderr, in yellow when stderr is a color-capable
/// terminal.
fn warn(msg: &str) {
    eprintln!("{}", colorize_yellow(msg, stderr_supports_color()));
}

/// Wrap `msg` in the ANSI yellow color when `enabled`, otherwise return it
/// unchanged. A single color pair spans the whole (possibly multi-line) message.
fn colorize_yellow(msg: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[33m{msg}\x1b[0m")
    } else {
        msg.to_string()
    }
}

/// Whether stderr should be colorized: it is a terminal and `NO_COLOR` is unset
/// or empty (per <https://no-color.org>).
fn stderr_supports_color() -> bool {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    !no_color && std::io::stderr().is_terminal()
}

/// Format a Unix timestamp as a UTC calendar date.
fn format_date(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(total_files: usize, parse_failures: Vec<String>) -> AnalysisResult {
        AnalysisResult {
            commit_count: 3,
            period: None,
            glob_filtered: false,
            total_files,
            parse_failures,
            files: Vec::new(),
            symbols: Vec::new(),
        }
    }

    #[test]
    fn json_reports_total_files_and_parse_failures() {
        let json = to_json(&result(10, vec!["a.go".to_string(), "b.ts".to_string()]));
        assert_eq!(json["total_files"], 10);
        assert_eq!(json["parse_failures"]["count"], 2);
        assert_eq!(json["parse_failures"]["files"][0], "a.go");
        assert_eq!(json["parse_failures"]["files"][1], "b.ts");
    }

    #[test]
    fn colorize_wraps_only_when_enabled() {
        assert_eq!(colorize_yellow("hi", true), "\x1b[33mhi\x1b[0m");
        assert_eq!(colorize_yellow("hi", false), "hi");
    }

    #[test]
    fn parse_failure_message_summarizes_and_lists_files() {
        let msg = parse_failure_message(&result(10, vec!["a.go".to_string(), "b.ts".to_string()]));
        assert_eq!(
            msg,
            "warning: failed to parse 2 of 10 changed file(s); their changes are counted at the \
             file level only:\n  - a.go\n  - b.ts"
        );
    }

    #[test]
    fn json_parse_failures_present_but_empty_when_all_parsed() {
        let json = to_json(&result(5, Vec::new()));
        assert_eq!(json["total_files"], 5);
        assert_eq!(json["parse_failures"]["count"], 0);
        assert_eq!(json["parse_failures"]["files"].as_array().unwrap().len(), 0);
    }
}
