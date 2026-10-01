//! Shared scaffolding for the tree-sitter based analyzers (Go, Kotlin, Scala).
//!
//! Each language module keeps only its `visit` match over grammar-specific
//! node kinds plus its own helpers. The walk state, symbol recording, and the
//! strict parse policy live here so a fix lands in one place.

use anyhow::{Result, anyhow};
use tree_sitter::{Node, Tree};

use super::Symbol;

/// Parse `source` with `language`, rejecting syntactically broken input.
///
/// tree-sitter is error-tolerant and always returns a tree. Every tree-sitter
/// analyzer rejects input containing error nodes, so the engine falls back to
/// file-level counting rather than trusting a partial parse.
pub(super) fn parse_strict(
    language: &tree_sitter::Language,
    language_name: &str,
    source: &str,
) -> Result<Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(language)
        .map_err(|e| anyhow!("failed to load {language_name} grammar: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("failed to parse {language_name} source"))?;
    if tree.root_node().has_error() {
        return Err(anyhow!("{language_name} source has syntax errors"));
    }
    Ok(tree)
}

/// Walk state shared by the tree-sitter analyzers.
pub(super) struct Walk<'a> {
    pub src: &'a str,
    pub symbols: Vec<Symbol>,
    /// Names of the scopes (types, receivers, enum entries) we are currently
    /// inside, outermost first. Used to qualify recorded names.
    pub scope_stack: Vec<String>,
    /// Name to attach to the next function literal we descend into (set when
    /// entering a binding or a call's argument list).
    pub name_hint: Option<String>,
    /// Number of enclosing function bodies we are currently inside.
    pub depth: u32,
}

impl<'a> Walk<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src,
            symbols: Vec::new(),
            scope_stack: Vec::new(),
            name_hint: None,
            depth: 0,
        }
    }
}

/// A per-language CST visitor over the shared [`Walk`] state. Implementors
/// provide `visit`; the provided methods supply recording and traversal.
pub(super) trait Visitor<'a> {
    fn walk(&self) -> &Walk<'a>;
    fn walk_mut(&mut self) -> &mut Walk<'a>;
    fn visit(&mut self, node: Node<'a>);

    fn text(&self, node: Node) -> String {
        self.walk().src[node.byte_range()].to_string()
    }

    fn record(&mut self, name: String, node: Node) {
        let walk = self.walk_mut();
        let qualified = if walk.scope_stack.is_empty() {
            name
        } else {
            format!("{}.{}", walk.scope_stack.join("."), name)
        };
        // tree-sitter rows are 0-based; the range is inclusive of the line
        // that holds the last token of the definition. The node's own end is
        // not used directly because a node without a closing brace (a Scala
        // indented body) swallows trailing blank lines and comments, which
        // would bleed the range into the next definition.
        let start_line = node.start_position().row as u32 + 1;
        let end_line = content_end_row(node) as u32 + 1;
        let depth = walk.depth + 1;
        walk.symbols.push(Symbol {
            name: qualified,
            start_line,
            end_line: end_line.max(start_line),
            // The function being recorded sits one level below its enclosers.
            depth,
        });
    }

    /// Record `name` for `node`, then walk its children with the depth bumped
    /// by one. The name hint is cleared for the body so it cannot leak onto a
    /// nested function literal, and restored afterwards.
    fn record_and_descend(&mut self, name: String, node: Node<'a>) {
        self.record(name, node);
        let saved = self.walk_mut().name_hint.take();
        self.walk_mut().depth += 1;
        self.visit_children(node);
        self.walk_mut().depth -= 1;
        self.walk_mut().name_hint = saved;
    }

    /// Record a function literal under the pending name hint, falling back to
    /// the given placeholder, then descend into it.
    fn record_hinted(&mut self, node: Node<'a>, fallback: &str) {
        let name = self
            .walk_mut()
            .name_hint
            .take()
            .unwrap_or_else(|| fallback.to_string());
        self.record_and_descend(name, node);
    }

    fn visit_children(&mut self, node: Node<'a>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child);
        }
    }

    /// Visit `node`'s children with `name` (when present) pushed as a scope
    /// segment. Scopes qualify names but add no nesting depth.
    fn visit_in_scope(&mut self, name: Option<String>, node: Node<'a>) {
        let pushed = name.is_some();
        if let Some(name) = name {
            self.walk_mut().scope_stack.push(name);
        }
        self.visit_children(node);
        if pushed {
            self.walk_mut().scope_stack.pop();
        }
    }

    /// Run `f` with the name hint set to `hint`, restoring the previous hint
    /// afterwards.
    fn with_hint(&mut self, hint: Option<String>, f: impl FnOnce(&mut Self))
    where
        Self: Sized,
    {
        let saved = self.walk_mut().name_hint.take();
        self.walk_mut().name_hint = hint;
        f(self);
        self.walk_mut().name_hint = saved;
    }
}

/// The 0-based row of `node`'s last own token, skipping trailing extras: the
/// deepest last non-extra descendant's end. Trailing comments and blank lines
/// absorbed by a brace-less node are not part of its definition.
fn content_end_row(node: Node) -> usize {
    let mut n = node;
    loop {
        let mut cursor = n.walk();
        let last = n.children(&mut cursor).filter(|c| !c.is_extra()).last();
        match last {
            Some(child) => n = child,
            None => return n.end_position().row,
        }
    }
}

/// The first named child of `node` with the given kind, if any.
pub(super) fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|c| c.kind() == kind)
}
