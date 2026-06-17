//! Language plugin mechanism.
//!
//! A [`LanguageAnalyzer`] knows how to parse a source file of one language and
//! report the function / method definitions it contains, each with the line
//! range it spans. The [`AnalyzerRegistry`] dispatches a file to the right
//! analyzer based on its extension. New languages are added by implementing the
//! trait and registering it — nothing else in the codebase needs to change.

use anyhow::Result;

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
    /// Human-readable language name. Part of the plugin API; surfaced when
    /// listing the registered analyzers.
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    /// File extensions (lower-case, without the leading dot) this analyzer handles.
    fn extensions(&self) -> &'static [&'static str];

    /// Extract every function / method defined in `source`. `path` is provided
    /// so analyzers can tune parsing per file kind (e.g. `.ts` vs `.tsx`).
    fn extract_symbols(&self, path: &str, source: &str) -> Result<Vec<Symbol>>;
}

/// Maps file extensions to the language analyzer that handles them.
pub struct AnalyzerRegistry {
    analyzers: Vec<Box<dyn LanguageAnalyzer>>,
}

impl AnalyzerRegistry {
    /// A registry pre-loaded with every analyzer shipped with hotcarpet.
    pub fn with_builtins() -> Self {
        let mut registry = Self {
            analyzers: Vec::new(),
        };
        registry.register(Box::new(typescript::TypeScriptAnalyzer));
        registry
    }

    /// Add a language analyzer. Later registrations take lower priority: the
    /// first analyzer claiming an extension wins.
    pub fn register(&mut self, analyzer: Box<dyn LanguageAnalyzer>) {
        self.analyzers.push(analyzer);
    }

    /// The analyzer responsible for `path`, if any.
    pub fn for_path(&self, path: &str) -> Option<&dyn LanguageAnalyzer> {
        let ext = extension_of(path)?;
        self.analyzers
            .iter()
            .find(|a| a.extensions().contains(&ext.as_str()))
            .map(|a| a.as_ref())
    }
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
}
