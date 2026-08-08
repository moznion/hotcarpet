//! Scala analyzer built on the [tree-sitter](https://tree-sitter.github.io)
//! Scala grammar (Scala 2 and 3, brace and indentation syntax).
//!
//! It walks the concrete syntax tree and records every `def`, lambda, and
//! partial-function literal together with the source line range it spans. A
//! lambda bound to a name (`val f = x => …`) inherits that name so the
//! leaderboard stays readable. One passed to a call is named after its callee,
//! whether applied with dot, infix, curried, or Scala 3 colon syntax, so
//! `xs.map(x => …)` and `xs.map: x =>` both become `map()`. An unnamed one is
//! recorded as `<lambda>` (`<partial>` for a bare `{ case … }` literal). A
//! secondary constructor (`def this(..)`) is recorded as `<constructor>`. A
//! def whose body is itself a function literal (`def receive = { case … }`)
//! is recorded once, under the def's name.
//!
//! Methods are qualified by the enclosing `class` / `object` / `trait` /
//! `enum` / `given` / `package object` names, e.g. `Outer.Inner.method`. An
//! anonymous class body (`new Type { … }`) and an enum case body add the type
//! or case name as a segment. Entering a type qualifies names but does not
//! increase nesting depth. Only stepping into a function body does. Bodiless
//! abstract `def`s carry no code, so they are not recorded.

use anyhow::Result;
use tree_sitter::Node;

use super::treesitter::{Visitor, Walk, child_of_kind, parse_strict};
use super::{LanguageAnalyzer, Symbol};

pub struct ScalaAnalyzer;

impl LanguageAnalyzer for ScalaAnalyzer {
    fn name(&self) -> &'static str {
        "Scala"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["scala", "sc"]
    }

    fn extract_symbols(&self, _path: &str, source: &str) -> Result<Vec<Symbol>> {
        let tree = parse_strict(&tree_sitter_scala::LANGUAGE.into(), "Scala", source)?;
        let mut collector = SymbolCollector {
            walk: Walk::new(source),
        };
        collector.visit(tree.root_node());
        Ok(collector.walk.symbols)
    }
}

/// Syntax-tree visitor that accumulates def / lambda / partial-function
/// symbols. The scope stack carries the names of the types we are inside; the
/// name hint is set when entering a `val` / `var` binding or a call's argument
/// list.
struct SymbolCollector<'a> {
    walk: Walk<'a>,
}

impl<'a> Visitor<'a> for SymbolCollector<'a> {
    fn walk(&self) -> &Walk<'a> {
        &self.walk
    }

    fn walk_mut(&mut self) -> &mut Walk<'a> {
        &mut self.walk
    }

    fn visit(&mut self, node: Node<'a>) {
        match node.kind() {
            // Types qualify the names of everything inside them but, like a
            // class in the TypeScript analyzer, add no nesting depth. An enum
            // case scopes its body so two cases overriding the same member
            // stay distinct.
            "class_definition" | "object_definition" | "trait_definition" | "enum_definition"
            | "given_definition" | "package_object" | "simple_enum_case" | "full_enum_case" => {
                let name = node.child_by_field_name("name").map(|n| self.text(n));
                self.visit_in_scope(name, node);
            }
            // An anonymous class body (`new Type { … }`) scopes its members
            // under the type name, keeping them apart from the enclosing
            // type's own members of the same name.
            "instance_expression" => {
                let Some(body) = child_of_kind(node, "template_body") else {
                    self.visit_children(node);
                    return;
                };
                let name = self
                    .instance_supertype(node)
                    .unwrap_or_else(|| "<anon>".to_string());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.id() == body.id() {
                        self.visit_in_scope(Some(name.clone()), child);
                    } else {
                        self.visit(child);
                    }
                }
            }
            // A `def` with a body. A bodiless abstract `def` is a separate kind
            // (`function_declaration`) and is intentionally not recorded.
            "function_definition" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n))
                    .unwrap_or_else(|| "<def>".to_string());
                // `def this(..)` is a secondary constructor. Recording it as
                // `this` would make an unreadable leaderboard entry.
                let name = if name == "this" {
                    "<constructor>".to_string()
                } else {
                    name
                };
                self.record_and_descend(name, node);
            }
            "lambda_expression" => {
                if is_function_body(node) {
                    self.visit_children(node);
                } else {
                    self.record_hinted(node, "<lambda>");
                }
            }
            // A `case_block` / `indented_cases` is a `match` body, a `catch`'s
            // handlers, a def's own body, or a partial-function literal
            // (`xs.collect { case … }`). Only the last is a function of its own.
            "case_block" | "indented_cases" => {
                let handled = matches!(
                    node.parent().map(|p| p.kind()),
                    Some("match_expression" | "catch_clause")
                ) || is_function_body(node);
                if handled {
                    self.visit_children(node);
                } else {
                    self.record_hinted(node, "<partial>");
                }
            }
            // Scala 3 fluent colon syntax. In `xs.map: x =>` plus an indented
            // body the closure has no lambda_expression node of its own (the
            // parameters land in the colon_argument's `lambda_start` field),
            // so the colon_argument is the function literal to record.
            "colon_argument" => {
                if node.child_by_field_name("lambda_start").is_some() {
                    self.record_hinted(node, "<lambda>");
                } else {
                    self.visit_children(node);
                }
            }
            // `val f = …` / `var f = …` — hint the right-hand side with the
            // bound name (when it is a plain or operator identifier pattern).
            "val_definition" | "var_definition" => {
                let hint = node
                    .child_by_field_name("pattern")
                    .filter(|p| matches!(p.kind(), "identifier" | "operator_identifier"))
                    .map(|p| self.text(p));
                if let Some(value) = node.child_by_field_name("value") {
                    self.with_hint(hint, |s| s.visit(value));
                }
            }
            // Name a call's function-literal arguments after their callee, e.g.
            // a lambda passed to `xs.map(...)` becomes `map()`.
            "call_expression" => {
                let func = node.child_by_field_name("function");
                if let Some(func) = func {
                    self.visit(func);
                }
                if let Some(args) = node.child_by_field_name("arguments") {
                    let callee = func.and_then(|f| self.callee_name(f));
                    self.with_hint(callee.map(|c| format!("{c}()")), |s| s.visit(args));
                }
            }
            // Operator-notation calls: `xs foreach { x => … }` — the literal
            // on the right is named after the infix function, like the dot
            // form.
            "infix_expression" => {
                let (left, op, right) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("operator"),
                    node.child_by_field_name("right"),
                );
                if let (Some(left), Some(op), Some(right)) = (left, op, right) {
                    self.visit(left);
                    let callee = matches!(op.kind(), "identifier" | "operator_identifier")
                        .then(|| format!("{}()", self.text(op)));
                    self.with_hint(callee, |s| s.visit(right));
                } else {
                    self.visit_children(node);
                }
            }
            _ => self.visit_children(node),
        }
    }
}

impl<'a> SymbolCollector<'a> {
    /// The simple name of a call's callee: `foo` for `foo(...)`, the trailing
    /// field for member calls like `xs.map(...)`, the base function for a
    /// generic call `f[T](...)` and for a curried call `f(a)(b)`. `None` for
    /// anything else.
    fn callee_name(&self, node: Node) -> Option<String> {
        match node.kind() {
            "identifier" | "operator_identifier" => Some(self.text(node)),
            "field_expression" => node.child_by_field_name("field").map(|f| self.text(f)),
            // A curried application (`xs.foldLeft(0)(f)`) nests the earlier
            // argument lists in the function position, so recurse to the base
            // callee.
            "generic_function" | "call_expression" => node
                .child_by_field_name("function")
                .and_then(|f| self.callee_name(f)),
            _ => None,
        }
    }

    /// The type an anonymous `new Type { … }` instance implements, reduced to
    /// its bare rightmost name.
    fn instance_supertype(&self, node: Node) -> Option<String> {
        let mut cursor = node.walk();
        let ty = node
            .named_children(&mut cursor)
            .find(|c| !matches!(c.kind(), "template_body" | "arguments"))?;
        Some(self.type_base_name(ty))
    }

    /// The bare rightmost name of a type expression: `Runnable` for
    /// `a.b.Runnable`, `Handler` for `Handler[Int]`.
    fn type_base_name(&self, node: Node) -> String {
        match node.kind() {
            "generic_type" | "compound_type" => node
                .named_child(0)
                .map(|n| self.type_base_name(n))
                .unwrap_or_else(|| self.text(node)),
            "stable_type_identifier" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .last()
                    .map(|n| self.text(n))
                    .unwrap_or_else(|| self.text(node))
            }
            _ => self.text(node),
        }
    }
}

/// Whether `node` is the body of an enclosing `function_definition`
/// (`def f = { case … }`, `def f = x => …`). Such a node spans the same lines
/// as the def, which is already recorded, so recording it again would only
/// shadow the def in innermost attribution.
fn is_function_body(node: Node) -> bool {
    node.parent().is_some_and(|p| {
        p.kind() == "function_definition"
            && p.child_by_field_name("body")
                .is_some_and(|b| b.id() == node.id())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> Vec<String> {
        let mut s = ScalaAnalyzer
            .extract_symbols("Test.scala", source)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect::<Vec<_>>();
        s.sort();
        s
    }

    fn symbol_named(source: &str, name: &str) -> Option<Symbol> {
        ScalaAnalyzer
            .extract_symbols("Test.scala", source)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
    }

    #[test]
    fn extracts_methods_qualified_by_type() {
        let src = r#"
            class Greeter(name: String) {
              def greet(who: String): String = "hi " + who
            }
            object Util {
              def run(): Unit = ()
            }
            trait Speaker {
              def speak(): String = "..."
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"Greeter.greet".to_string()));
        assert!(got.contains(&"Util.run".to_string()));
        assert!(got.contains(&"Speaker.speak".to_string()));
    }

    #[test]
    fn qualifies_nested_types() {
        let src = r#"
            object Outer {
              object Inner {
                def leaf(): Int = 1
              }
            }
        "#;
        assert!(names(src).contains(&"Outer.Inner.leaf".to_string()));
    }

    #[test]
    fn abstract_defs_are_not_recorded() {
        // A bodiless `def` carries no code, like a Go interface signature.
        let src = r#"
            trait T {
              def abstractDef(x: Int): Int
              def concrete(): Int = 1
            }
        "#;
        let got = names(src);
        assert!(!got.contains(&"T.abstractDef".to_string()));
        assert!(got.contains(&"T.concrete".to_string()));
    }

    #[test]
    fn tracks_function_nesting_depth() {
        let src = r#"
            object O {
              def outer(): Int = {
                def inner(): Int = {
                  def deepest(): Int = 1
                  deepest()
                }
                inner()
              }
            }
        "#;
        let depth_of = |name: &str| symbol_named(src, name).map(|s| s.depth);
        assert_eq!(depth_of("O.outer"), Some(1));
        assert_eq!(depth_of("O.inner"), Some(2));
        assert_eq!(depth_of("O.deepest"), Some(3));
    }

    #[test]
    fn labels_lambdas_from_bindings_and_callees() {
        let src = r#"
            object O {
              def run(): Unit = {
                val double = (x: Int) => x * 2
                List(1).map(x => x + 1)
                List(1).foreach { x => println(x) }
              }
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"O.double".to_string()));
        assert!(got.contains(&"O.map()".to_string()));
        assert!(got.contains(&"O.foreach()".to_string()));
    }

    #[test]
    fn labels_partial_function_literals() {
        let src = r#"
            object O {
              def f(xs: List[Int]): List[Int] = xs.collect { case n if n > 0 => n }
              val pf: PartialFunction[Int, Int] = { case n => n }
            }
        "#;
        // The exact set matters: no phantom `<partial>` rows may accompany
        // the named ones.
        let got = names(src);
        assert_eq!(
            got,
            vec![
                "O.collect()".to_string(),
                "O.f".to_string(),
                "O.pf".to_string()
            ]
        );
    }

    #[test]
    fn def_bodied_literals_record_only_the_def() {
        let src = r#"
            object O {
              def receive: PartialFunction[Any, Unit] = {
                case s: String => println(s)
              }
              def f: Int => Int = x => x + 1
            }
        "#;
        // The literal IS the def's body: recording it separately would shadow
        // the def in innermost attribution.
        assert_eq!(names(src), vec!["O.f".to_string(), "O.receive".to_string()]);
    }

    #[test]
    fn labels_colon_argument_lambdas() {
        // Scala 3 fluent syntax: the closure after `map:` has no
        // lambda_expression node of its own.
        let src = "def f(xs: List[Int]) =\n  xs.map: x =>\n    x + 1\n";
        assert!(names(src).contains(&"map()".to_string()));
    }

    #[test]
    fn labels_curried_call_lambdas() {
        let src = r#"
            object O {
              def s(xs: List[Int]): Int = xs.foldLeft(0)((acc, x) => acc + x)
              def t(xs: List[Int]): Int = xs.foldLeft(1) { (acc, x) => acc * x }
            }
        "#;
        let got = names(src);
        assert_eq!(got.iter().filter(|n| *n == "O.foldLeft()").count(), 2);
    }

    #[test]
    fn labels_infix_call_literals() {
        let src = r#"
            object O {
              def f(xs: List[Int]): Unit = xs foreach { x => println(x) }
              def g(xs: List[Int]): List[Int] = xs collect { case n => n }
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"O.foreach()".to_string()));
        assert!(got.contains(&"O.collect()".to_string()));
    }

    #[test]
    fn labels_operator_named_lambda_vals() {
        let src = r#"
            object O {
              val ++ = (a: Int) => a + 1
            }
        "#;
        assert!(names(src).contains(&"O.++".to_string()));
    }

    #[test]
    fn qualifies_given_members() {
        let src = r#"
            given intOrd: Ordering[Int] with {
              def compare(a: Int, b: Int): Int = a - b
            }
        "#;
        assert!(names(src).contains(&"intOrd.compare".to_string()));
    }

    #[test]
    fn qualifies_package_object_members() {
        let src = r#"
            package object util {
              def helper: Int = 1
            }
        "#;
        assert!(names(src).contains(&"util.helper".to_string()));
    }

    #[test]
    fn qualifies_enum_case_body_members() {
        let src = r#"
            enum Color {
              case Red extends Color {
                def d: Int = 1
              }
              case Green
            }
        "#;
        assert!(names(src).contains(&"Color.Red.d".to_string()));
    }

    #[test]
    fn scopes_anonymous_class_members_by_type() {
        let src = r#"
            object O {
              def run(): Unit = ()
              val r = new Runnable {
                def run(): Unit = ()
              }
            }
        "#;
        // The anonymous instance's method must not merge with the enclosing
        // object's own `run`.
        let got = names(src);
        assert!(got.contains(&"O.run".to_string()));
        assert!(got.contains(&"O.Runnable.run".to_string()));
    }

    #[test]
    fn indented_body_range_stops_at_last_content_line() {
        // A brace-less body has no closing token, so its node swallows the
        // trailing blank line and the next member's doc comment. Neither may
        // count toward the def's range.
        let src = "object O:\n  def a(): Int =\n    1\n\n  /** doc */\n  def b(): Int =\n    2\n";
        let a = symbol_named(src, "O.a").unwrap();
        assert_eq!((a.start_line, a.end_line), (2, 3));
        let b = symbol_named(src, "O.b").unwrap();
        assert_eq!((b.start_line, b.end_line), (6, 7));
    }

    #[test]
    fn match_bodies_are_not_functions() {
        let src = r#"
            object O {
              def g(x: Int): Int = x match {
                case 0 => 0
                case _ => 1
              }
            }
        "#;
        // Only the enclosing def is recorded; the match's case block is not a
        // partial-function literal.
        assert_eq!(names(src), vec!["O.g".to_string()]);
    }

    #[test]
    fn secondary_constructor_is_labelled() {
        let src = r#"
            class C(x: Int) {
              def this() = this(0)
            }
        "#;
        assert!(names(src).contains(&"C.<constructor>".to_string()));
    }

    #[test]
    fn extracts_enum_methods() {
        let src = r#"
            enum Color {
              case Red, Green
              def describe(): String = "c"
            }
        "#;
        assert!(names(src).contains(&"Color.describe".to_string()));
    }

    #[test]
    fn extracts_top_level_defs() {
        // Scala 3 allows top-level definitions; they are unqualified.
        let src = "def top(): Int = 1\n";
        assert!(names(src).contains(&"top".to_string()));
    }

    #[test]
    fn records_multi_line_method_range() {
        let src = "object O {\n  def m(): Int = {\n    1\n  }\n}\n";
        let m = symbol_named(src, "O.m").unwrap();
        assert_eq!(m.start_line, 2);
        assert_eq!(m.end_line, 4);
    }

    #[test]
    fn invalid_source_is_an_error() {
        assert!(
            ScalaAnalyzer
                .extract_symbols("Bad.scala", "object O { def f( = ")
                .is_err()
        );
    }

    #[test]
    fn empty_and_declaration_only_sources_have_no_symbols() {
        assert!(
            ScalaAnalyzer
                .extract_symbols("Empty.scala", "")
                .unwrap()
                .is_empty()
        );
        assert!(
            ScalaAnalyzer
                .extract_symbols("Data.scala", "case class P(x: Int, y: Int)\n")
                .unwrap()
                .is_empty()
        );
    }
}
