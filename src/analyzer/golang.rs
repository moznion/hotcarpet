//! Go analyzer built on the [tree-sitter](https://tree-sitter.github.io) Go
//! grammar.
//!
//! It walks the concrete syntax tree and records every function, method, and
//! function literal together with the source line range it spans. A function
//! literal bound to a name (`f := func() {}`, `var f = func() {}`, a struct
//! field, or the argument of a call) inherits that name so the leaderboard stays
//! readable; an unnamed one is recorded as `<closure>`.
//!
//! Methods are qualified by their receiver type, e.g. `Greeter.Greet` for a
//! `Greet` method on `*Greeter`. Entering a method qualifies the names of the
//! closures it contains — like a class in the TypeScript analyzer — but does not
//! increase nesting depth; only stepping into a function body does.

use anyhow::{Result, anyhow};
use tree_sitter::Node;

use super::{LanguageAnalyzer, Symbol};

pub struct GoAnalyzer;

impl LanguageAnalyzer for GoAnalyzer {
    fn name(&self) -> &'static str {
        "Go"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["go"]
    }

    fn extract_symbols(&self, _path: &str, source: &str) -> Result<Vec<Symbol>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .map_err(|e| anyhow!("failed to load Go grammar: {e}"))?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow!("failed to parse Go source"))?;
        let root = tree.root_node();
        // tree-sitter is error-tolerant and always returns a tree. Match the
        // other analyzers, which reject syntactically broken input so the engine
        // falls back to file-level counting rather than trusting a partial parse.
        if root.has_error() {
            return Err(anyhow!("Go source has syntax errors"));
        }

        let mut collector = SymbolCollector {
            src: source,
            symbols: Vec::new(),
            scope_stack: Vec::new(),
            name_hint: None,
            depth: 0,
        };
        collector.visit(root);
        Ok(collector.symbols)
    }
}

/// Syntax-tree visitor that accumulates function / method / closure symbols.
struct SymbolCollector<'a> {
    src: &'a str,
    symbols: Vec<Symbol>,
    /// Receiver types of the methods we are currently inside, outermost first.
    /// Used to qualify recorded names.
    scope_stack: Vec<String>,
    /// Name to attach to the next function literal we descend into (set when
    /// entering a binding such as `f := ...`, a struct field, or a call argument).
    name_hint: Option<String>,
    /// Number of enclosing function bodies we are currently inside.
    depth: u32,
}

impl<'a> SymbolCollector<'a> {
    fn text(&self, node: Node) -> String {
        self.src[node.byte_range()].to_string()
    }

    fn record(&mut self, name: String, node: Node) {
        let qualified = if self.scope_stack.is_empty() {
            name
        } else {
            format!("{}.{}", self.scope_stack.join("."), name)
        };
        // tree-sitter rows are 0-based; the range is inclusive of the line that
        // holds the closing brace.
        let start_line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        self.symbols.push(Symbol {
            name: qualified,
            start_line,
            end_line,
            // The function being recorded sits one level below its enclosers.
            depth: self.depth + 1,
        });
    }

    /// Record `name` for `node`, then walk its children with the depth bumped by
    /// one. The name hint is cleared for the body so it cannot leak onto a nested
    /// closure, and restored afterwards.
    fn record_and_descend(&mut self, name: String, node: Node<'a>) {
        self.record(name, node);
        let saved = self.name_hint.take();
        self.depth += 1;
        self.visit_children(node);
        self.depth -= 1;
        self.name_hint = saved;
    }

    fn visit_children(&mut self, node: Node<'a>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child);
        }
    }

    fn visit(&mut self, node: Node<'a>) {
        match node.kind() {
            "function_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n))
                    .unwrap_or_else(|| "<func>".to_string());
                self.record_and_descend(name, node);
            }
            "method_declaration" => {
                let recv = self.receiver_type(node);
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n))
                    .unwrap_or_else(|| "<method>".to_string());
                // Push the receiver type so the method — and any closures inside
                // it — are qualified by it. Like a class, it adds no depth.
                if let Some(recv) = &recv {
                    self.scope_stack.push(recv.clone());
                }
                self.record_and_descend(name, node);
                if recv.is_some() {
                    self.scope_stack.pop();
                }
            }
            "func_literal" => {
                let name = self
                    .name_hint
                    .take()
                    .unwrap_or_else(|| "<closure>".to_string());
                self.record_and_descend(name, node);
            }
            // `f := func() {}` / `a, b := 1, func() {}` / `h = func() {}` — pair
            // each right-hand value with the identifier it is assigned to.
            "short_var_declaration" | "assignment_statement" => {
                self.visit_bindings(
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                );
            }
            // `var f = func() {}` / `var a, b = 1, func() {}` — the spec carries
            // one or more `name` children and a `value` expression list.
            "var_spec" | "const_spec" => {
                let names = self.field_children(node, "name");
                let values = node.child_by_field_name("value");
                self.pair_bindings(&names, values);
            }
            // Name a call's function-literal arguments after their callee, e.g.
            // an argument passed to `describe(...)` becomes `describe()`.
            "call_expression" => {
                if let Some(func) = node.child_by_field_name("function") {
                    self.visit(func);
                }
                if let Some(args) = node.child_by_field_name("arguments") {
                    let callee = node
                        .child_by_field_name("function")
                        .and_then(|f| self.callee_name(f));
                    let saved = self.name_hint.take();
                    self.name_hint = callee.map(|c| format!("{c}()"));
                    self.visit_children(args);
                    self.name_hint = saved;
                }
            }
            // Struct-literal field holding a function: `T{ handler: func() {} }`.
            "keyed_element" => {
                let key = node
                    .child_by_field_name("key")
                    .and_then(|k| self.element_ident(k));
                if let Some(value) = node.child_by_field_name("value") {
                    let saved = self.name_hint.take();
                    self.name_hint = key;
                    self.visit(value);
                    self.name_hint = saved;
                }
            }
            _ => self.visit_children(node),
        }
    }

    /// Walk an assignment/short-var declaration, hinting each right-hand value
    /// with the name it is bound to.
    fn visit_bindings(&mut self, left: Option<Node<'a>>, right: Option<Node<'a>>) {
        let names: Vec<Node> = left.map(|l| named_children(l)).unwrap_or_default();
        self.pair_bindings(&names, right);
    }

    /// Visit each value in `values`, hinting the i-th value with the i-th name
    /// (when that name is a plain identifier).
    fn pair_bindings(&mut self, names: &[Node<'a>], values: Option<Node<'a>>) {
        let Some(values) = values else { return };
        let mut cursor = values.walk();
        for (i, value) in values.named_children(&mut cursor).enumerate() {
            let hint = names
                .get(i)
                .filter(|n| n.kind() == "identifier")
                .map(|n| self.text(*n));
            let saved = self.name_hint.take();
            self.name_hint = hint;
            self.visit(value);
            self.name_hint = saved;
        }
    }

    /// The receiver type name of a method, with any leading `*` and generic type
    /// arguments stripped: `Greeter` for `(*Greeter)`, `Stack` for `(*Stack[T])`.
    fn receiver_type(&self, node: Node) -> Option<String> {
        let recv = node.child_by_field_name("receiver")?;
        let mut cursor = recv.walk();
        let decl = recv
            .named_children(&mut cursor)
            .find(|n| n.kind() == "parameter_declaration")?;
        let typ = decl.child_by_field_name("type")?;
        Some(self.type_name(typ))
    }

    /// The bare name of a (possibly pointer/generic) type expression.
    fn type_name(&self, node: Node) -> String {
        match node.kind() {
            "pointer_type" => node
                .named_child(0)
                .map(|n| self.type_name(n))
                .unwrap_or_else(|| "<type>".to_string()),
            "generic_type" => node
                .child_by_field_name("type")
                .map(|n| self.type_name(n))
                .unwrap_or_else(|| "<type>".to_string()),
            _ => self.text(node),
        }
    }

    /// The simple name of a call's callee: `foo` for `foo(...)`, the trailing
    /// field for member calls like `x.y.foo(...)`. `None` for anything else.
    fn callee_name(&self, node: Node) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text(node)),
            "selector_expression" => node.child_by_field_name("field").map(|f| self.text(f)),
            _ => None,
        }
    }

    /// The identifier a composite-literal key wraps: `handler` for the key of
    /// `handler: func() {}`. Keys are `literal_element` nodes around an ident.
    fn element_ident(&self, node: Node) -> Option<String> {
        let inner = if node.kind() == "literal_element" {
            node.named_child(0)?
        } else {
            node
        };
        (inner.kind() == "identifier").then(|| self.text(inner))
    }

    /// All children of `node` carried in field `field`, in order.
    fn field_children(&self, node: Node<'a>, field: &str) -> Vec<Node<'a>> {
        let mut cursor = node.walk();
        let mut out = Vec::new();
        if cursor.goto_first_child() {
            loop {
                if cursor.field_name() == Some(field) {
                    out.push(cursor.node());
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        out
    }
}

/// The named (non-punctuation) children of a node, in order.
fn named_children<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> Vec<String> {
        let mut s = GoAnalyzer
            .extract_symbols("test.go", source)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect::<Vec<_>>();
        s.sort();
        s
    }

    fn symbol_named(source: &str, name: &str) -> Option<Symbol> {
        GoAnalyzer
            .extract_symbols("test.go", source)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
    }

    #[test]
    fn extracts_functions_and_methods() {
        let src = r#"
            package main
            func Top() int { return 1 }
            type Greeter struct{ name string }
            func (g *Greeter) Greet(who string) string { return "hi " + who }
            func (g Greeter) Name() string { return g.name }
        "#;
        let got = names(src);
        assert!(got.contains(&"Top".to_string()));
        assert!(got.contains(&"Greeter.Greet".to_string()));
        assert!(got.contains(&"Greeter.Name".to_string()));
    }

    #[test]
    fn labels_closures_from_bindings() {
        let src = r#"
            package main
            func run() {
                doubler := func(x int) int { return x * 2 }
                var named = func() {}
                do(func() {})
                _ = doubler
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"doubler".to_string()));
        assert!(got.contains(&"named".to_string()));
        // The literal passed to `do` is named after its callee.
        assert!(got.contains(&"do()".to_string()));
    }

    #[test]
    fn labels_bare_closure() {
        let src = r#"
            package main
            func run() {
                go func() {}()
            }
        "#;
        assert!(names(src).contains(&"<closure>".to_string()));
    }

    #[test]
    fn qualifies_closures_inside_methods() {
        let src = r#"
            package main
            type S struct{}
            func (s S) Run() {
                cb := func() int { return 1 }
                _ = cb
            }
        "#;
        // The method is depth 1 (the receiver adds no level); the closure it
        // holds is depth 2 and inherits the receiver-type qualification.
        assert_eq!(symbol_named(src, "S.Run").map(|s| s.depth), Some(1));
        assert_eq!(symbol_named(src, "S.cb").map(|s| s.depth), Some(2));
    }

    #[test]
    fn tracks_function_nesting_depth() {
        let src = r#"
            package main
            func outer() {
                inner := func() {
                    deepest := func() int { return 1 }
                    _ = deepest
                }
                _ = inner
            }
        "#;
        let syms = GoAnalyzer.extract_symbols("d.go", src).unwrap();
        let depth_of = |name: &str| syms.iter().find(|s| s.name == name).map(|s| s.depth);
        assert_eq!(depth_of("outer"), Some(1));
        assert_eq!(depth_of("inner"), Some(2));
        assert_eq!(depth_of("deepest"), Some(3));
    }

    #[test]
    fn extracts_generic_functions() {
        // A type-parameter list sits between the name and the body; the name is
        // still recorded.
        let src = r#"
            package main
            func Map[T any, U any](s []T, f func(T) U) []U { return nil }
        "#;
        assert!(names(src).contains(&"Map".to_string()));
    }

    #[test]
    fn labels_closure_from_selector_callee() {
        // A literal passed to a member call `t.Run(...)` is named after the
        // trailing selector segment, matching the TypeScript analyzer.
        let src = r#"
            package main
            func run(t T) {
                t.Run(func() {})
                pkg.sub.Do(func() {})
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"Run()".to_string()));
        assert!(got.contains(&"Do()".to_string()));
    }

    #[test]
    fn labels_closures_in_var_block() {
        // A grouped `var ( ... )` declaration wraps each spec in a var_spec_list;
        // the closure it binds is still named.
        let src = r#"
            package main
            var (
                first  = func() {}
                second = func() int { return 1 }
            )
        "#;
        let got = names(src);
        assert!(got.contains(&"first".to_string()));
        assert!(got.contains(&"second".to_string()));
    }

    #[test]
    fn interface_method_signatures_are_not_recorded() {
        // Interface methods are bodiless signatures (`method_elem`, not
        // `method_declaration`); dig-down tracks only things with a body, so they
        // are intentionally absent from the leaderboard.
        let src = r#"
            package main
            type Reader interface {
                Read(p []byte) (int, error)
            }
        "#;
        assert!(names(src).is_empty());
    }

    #[test]
    fn records_multi_line_method_range() {
        // The recorded span runs from the `func` keyword to the closing brace.
        let src = "package main\ntype C struct{}\nfunc (c C) M() int {\n    return 1\n}\n";
        let m = symbol_named(src, "C.M").unwrap();
        assert_eq!(m.start_line, 3);
        assert_eq!(m.end_line, 5);
    }

    #[test]
    fn strips_pointer_and_generic_receivers() {
        let src = r#"
            package main
            type Stack[T any] struct{ items []T }
            func (s *Stack[T]) Push(v T) { s.items = append(s.items, v) }
        "#;
        assert!(names(src).contains(&"Stack.Push".to_string()));
    }

    #[test]
    fn labels_struct_field_closures() {
        let src = r#"
            package main
            type Server struct{ handler func() }
            func build() Server {
                return Server{ handler: func() {} }
            }
        "#;
        assert!(names(src).contains(&"handler".to_string()));
    }

    #[test]
    fn records_plausible_line_ranges() {
        let src = "package main\nfunc a() int {\n    return 1\n}\n";
        let a = symbol_named(src, "a").unwrap();
        assert_eq!(a.start_line, 2);
        assert_eq!(a.end_line, 4);
    }

    #[test]
    fn package_level_closure_is_unqualified() {
        let src = "package main\nvar handler = func() {}\n";
        assert!(names(src).contains(&"handler".to_string()));
    }

    #[test]
    fn invalid_source_is_an_error() {
        assert!(
            GoAnalyzer
                .extract_symbols("bad.go", "package main\nfunc (")
                .is_err()
        );
    }

    #[test]
    fn empty_and_declaration_only_sources_have_no_symbols() {
        assert!(
            GoAnalyzer
                .extract_symbols("empty.go", "package main\n")
                .unwrap()
                .is_empty()
        );
        assert!(
            GoAnalyzer
                .extract_symbols(
                    "data.go",
                    "package main\ntype T struct{ x int }\nconst K = 1\n"
                )
                .unwrap()
                .is_empty()
        );
    }
}
