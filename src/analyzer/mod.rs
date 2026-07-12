//! Language plugin mechanism.
//!
//! A [`LanguageAnalyzer`] knows how to parse a source file of one language and
//! report the function / method definitions it contains, each with the line
//! range it spans. The [`AnalyzerRegistry`] dispatches a file to the right
//! analyzer based on its extension. New languages are added by implementing the
//! trait and registering it — nothing else in the codebase needs to change.

use std::collections::HashMap;

use anyhow::Result;

use crate::config::Config;

mod rust;
mod typescript;

/// A named source symbol (function or method) and the 1-based, inclusive line
/// range it occupies in the file.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// Qualified name, e.g. `func`, `Class.method`, `Outer.Inner.method`.
    pub name: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Function-nesting depth, 1-based. Top-level functions and the methods of
    /// a top-level class are depth 1; a function defined inside one is depth 2,
    /// and so on. Entering a class does not increase depth — only functions do.
    pub depth: u32,
}

impl Symbol {
    /// Whether this symbol's line range covers `line`.
    pub fn covers(&self, line: u32) -> bool {
        self.start_line <= line && line <= self.end_line
    }
}

/// A language plugin that extracts function / method spans from source code.
pub trait LanguageAnalyzer: Send + Sync {
    /// Human-readable language name. Also the key used to address this analyzer
    /// from a config file (matched case-insensitively, e.g. `typescript`).
    fn name(&self) -> &'static str;

    /// File extensions (lower-case, without the leading dot) this analyzer handles.
    fn extensions(&self) -> &'static [&'static str];

    /// Extract every function / method defined in `source`. `path` is provided
    /// so analyzers can tune parsing per file kind (e.g. `.ts` vs `.tsx`).
    fn extract_symbols(&self, path: &str, source: &str) -> Result<Vec<Symbol>>;
}

/// One registered analyzer together with the extensions it currently handles
/// (its built-in list, possibly overridden by config).
struct Entry {
    analyzer: Box<dyn LanguageAnalyzer>,
    extensions: Vec<String>,
}

/// Maps file extensions to the language analyzer that handles them.
pub struct AnalyzerRegistry {
    entries: Vec<Entry>,
    /// Extension (lower-case, no dot) -> index into `entries`. The first
    /// registration claiming an extension wins.
    by_ext: HashMap<String, usize>,
}

impl AnalyzerRegistry {
    /// A registry pre-loaded with every analyzer shipped with hotcarpet.
    pub fn with_builtins() -> Self {
        let mut registry = Self {
            entries: Vec::new(),
            by_ext: HashMap::new(),
        };
        registry.register(Box::new(typescript::TypeScriptAnalyzer));
        registry.register(Box::new(rust::RustAnalyzer));
        registry
    }

    /// Add a language analyzer, seeding its extensions from the analyzer's
    /// built-in list. Later registrations take lower priority: the first
    /// analyzer claiming an extension wins.
    pub fn register(&mut self, analyzer: Box<dyn LanguageAnalyzer>) {
        let extensions = analyzer
            .extensions()
            .iter()
            .filter_map(|e| normalize_ext(e))
            .collect();
        self.entries.push(Entry {
            analyzer,
            extensions,
        });
        self.rebuild_index();
    }

    /// Apply user configuration, overriding the extension mapping of named
    /// analyzers. `extensions` replaces an analyzer's list wholesale (omitted =
    /// keep the built-in list); `extra_extensions` are appended on top. Config
    /// entries naming an unknown analyzer are reported to stderr and skipped.
    pub fn apply_config(&mut self, config: &Config) {
        for (name, cfg) in &config.analyzers {
            let Some(entry) = self
                .entries
                .iter_mut()
                .find(|e| e.analyzer.name().eq_ignore_ascii_case(name))
            else {
                eprintln!("hotcarpet: config refers to unknown analyzer '{name}'; ignoring");
                continue;
            };

            let mut extensions: Vec<String> = match &cfg.extensions {
                Some(list) => list.iter().filter_map(|e| normalize_ext(e)).collect(),
                None => entry
                    .analyzer
                    .extensions()
                    .iter()
                    .filter_map(|e| normalize_ext(e))
                    .collect(),
            };
            extensions.extend(cfg.extra_extensions.iter().filter_map(|e| normalize_ext(e)));
            dedup_in_place(&mut extensions);
            entry.extensions = extensions;
        }
        self.rebuild_index();
    }

    /// The analyzer responsible for `path`, if any.
    pub fn for_path(&self, path: &str) -> Option<&dyn LanguageAnalyzer> {
        let ext = extension_of(path)?;
        self.by_ext
            .get(&ext)
            .map(|&i| self.entries[i].analyzer.as_ref())
    }

    /// Rebuild the extension index from the entries (first registration wins).
    fn rebuild_index(&mut self) {
        self.by_ext.clear();
        for (i, entry) in self.entries.iter().enumerate() {
            for ext in &entry.extensions {
                self.by_ext.entry(ext.clone()).or_insert(i);
            }
        }
    }
}

/// Normalize an extension: trim, drop a leading dot, lower-case. Empty -> None.
fn normalize_ext(ext: &str) -> Option<String> {
    let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    (!ext.is_empty()).then_some(ext)
}

/// Remove duplicate strings, keeping first occurrences (order-preserving).
fn dedup_in_place(items: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|x| seen.insert(x.clone()));
}

/// Lower-cased file extension (without dot) of `path`, if it has one.
fn extension_of(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Maps byte offsets within a source string to 1-based line numbers.
pub struct LineIndex {
    /// Byte offset at which each line starts. `line_starts[0]` is always 0.
    line_starts: Vec<u32>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        Self { line_starts }
    }

    /// 1-based line number containing byte `offset`.
    pub fn line_of(&self, offset: u32) -> u32 {
        match self.line_starts.binary_search(&offset) {
            // `offset` is exactly the start of line index `idx` (0-based).
            Ok(idx) => (idx as u32) + 1,
            // `idx` line starts are <= offset, so it falls on line `idx`.
            Err(idx) => idx as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_maps_offsets() {
        // "ab\ncd\n" -> line starts at bytes 0, 3, 6
        let idx = LineIndex::new("ab\ncd\n");
        assert_eq!(idx.line_of(0), 1);
        assert_eq!(idx.line_of(1), 1);
        assert_eq!(idx.line_of(2), 1); // the '\n' belongs to line 1
        assert_eq!(idx.line_of(3), 2);
        assert_eq!(idx.line_of(5), 2);
    }

    #[test]
    fn extension_lookup() {
        assert_eq!(extension_of("src/a.TS").as_deref(), Some("ts"));
        assert_eq!(extension_of("Makefile"), None);
    }

    use crate::config::{AnalyzerConfig, Config};

    fn config_with(name: &str, cfg: AnalyzerConfig) -> Config {
        let mut analyzers = HashMap::new();
        analyzers.insert(name.to_string(), cfg);
        Config { analyzers }
    }

    fn analyzer_name(registry: &AnalyzerRegistry, path: &str) -> Option<&'static str> {
        registry.for_path(path).map(|a| a.name())
    }

    #[test]
    fn builtin_extensions_route_to_typescript() {
        let registry = AnalyzerRegistry::with_builtins();
        assert_eq!(analyzer_name(&registry, "a.ts"), Some("TypeScript"));
        assert_eq!(analyzer_name(&registry, "a.vue"), None);
    }

    #[test]
    fn builtin_extensions_route_to_rust() {
        let registry = AnalyzerRegistry::with_builtins();
        assert_eq!(analyzer_name(&registry, "a.rs"), Some("Rust"));
    }

    #[test]
    fn extra_extensions_add_to_defaults() {
        let mut registry = AnalyzerRegistry::with_builtins();
        registry.apply_config(&config_with(
            // analyzer name is matched case-insensitively
            "typescript",
            AnalyzerConfig {
                extensions: None,
                extra_extensions: vec![".VUE".to_string()],
            },
        ));
        // the new extension is normalized and routed...
        assert_eq!(analyzer_name(&registry, "a.vue"), Some("TypeScript"));
        // ...and the built-in extensions still work.
        assert_eq!(analyzer_name(&registry, "a.ts"), Some("TypeScript"));
    }

    #[test]
    fn extensions_replace_defaults() {
        let mut registry = AnalyzerRegistry::with_builtins();
        registry.apply_config(&config_with(
            "TypeScript",
            AnalyzerConfig {
                extensions: Some(vec!["ts".to_string(), "tsx".to_string()]),
                extra_extensions: vec![],
            },
        ));
        assert_eq!(analyzer_name(&registry, "a.ts"), Some("TypeScript"));
        // `.js` was in the defaults but is dropped by the replacement.
        assert_eq!(analyzer_name(&registry, "a.js"), None);
    }

    #[test]
    fn unknown_analyzer_is_ignored() {
        let mut registry = AnalyzerRegistry::with_builtins();
        registry.apply_config(&config_with(
            "python",
            AnalyzerConfig {
                extensions: Some(vec!["py".to_string()]),
                extra_extensions: vec![],
            },
        ));
        // No analyzer named "python"; the entry is skipped, defaults unchanged.
        assert_eq!(analyzer_name(&registry, "a.py"), None);
        assert_eq!(analyzer_name(&registry, "a.ts"), Some("TypeScript"));
    }
}
