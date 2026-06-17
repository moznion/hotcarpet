//! Renders analysis results as JSON (default) or human-readable tables.

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
    // Always surface the "why is this empty" hint on stderr, regardless of format.
    if result.files.is_empty() {
        warn_empty(result);
    }
}

fn print_json(result: &AnalysisResult) {
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

    let out = json!({
        "commit_count": result.commit_count,
        "period": period,
        "files": files,
        "symbols": symbols,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
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

/// Emit a hint (to stderr) explaining why the leaderboard came out empty.
fn warn_empty(result: &AnalysisResult) {
    if result.commit_count == 0 {
        eprintln!("warning: no commits matched the selected time range.");
    } else if result.glob_filtered {
        eprintln!(
            "warning: analyzed {} commit(s) but no changed file matched the given glob(s).\n\
             hint: globs are matched against repo-root-relative paths (e.g. 'src/**/*.ts'); \
             quote them and drop any leading './'.",
            result.commit_count,
        );
    }
}

/// Format a Unix timestamp as a UTC calendar date.
fn format_date(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}
