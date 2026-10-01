//! Kotlin analyzer built on the [tree-sitter](https://tree-sitter.github.io)
//! Kotlin grammar (the `exoego/tree-sitter-kotlin` fork).
//!
//! It walks the concrete syntax tree and records every `fun`, lambda,
//! anonymous function, property accessor, secondary constructor, and `init`
//! block together with the source line range it spans. A lambda bound to a
//! property (`val f = { … }`) or held by a property delegate (`val f by
//! lazy { … }`) inherits that property's name so the leaderboard stays
//! readable. A lambda passed to a call is named after its callee, so
//! `xs.map { … }` and `xs map { … }` both become `map()`. An unnamed one is
//! recorded as `<lambda>` (`<anonymous>` for a bare `fun(…) { … }`). A
//! property accessor is recorded as `prop.get` / `prop.set`, a secondary
//! constructor as `<constructor>`, an `init` block as `<init>`.
//!
//! Methods are qualified by the enclosing `class` / `object` / `interface` /
//! `enum class` names, e.g. `Outer.Inner.method`. An enum entry with a body
//! and an anonymous `object : Type { … }` add a segment too (`E.ADD.apply`,
//! `C.Runnable.run`), while a `companion object` adds none, matching how its
//! members are called. Entering a type qualifies names but does not increase
//! nesting depth. Only stepping into a function body does. Bodiless
//! declarations (abstract / interface signatures) carry no code, so they are
//! not recorded.

use anyhow::Result;
use tree_sitter::Node;

use super::treesitter::{Visitor, Walk, child_of_kind, parse_strict};
use super::{LanguageAnalyzer, Symbol};

pub struct KotlinAnalyzer;

impl LanguageAnalyzer for KotlinAnalyzer {
    fn name(&self) -> &'static str {
        "Kotlin"
    }

    fn extensions(&self) -> &'static [&'static str] {
        // `.kts` is Kotlin script (e.g. Gradle build scripts), same grammar.
        &["kt", "kts"]
    }

    fn extract_symbols(&self, _path: &str, source: &str) -> Result<Vec<Symbol>> {
        let tree = parse_strict(&tree_sitter_kotlin::LANGUAGE.into(), "Kotlin", source)?;
        let mut collector = SymbolCollector {
            walk: Walk::new(source),
        };
        collector.visit(tree.root_node());
        Ok(collector.walk.symbols)
    }
}

/// Syntax-tree visitor that accumulates fun / lambda / accessor symbols. The
/// scope stack carries the names of the types (class / object / interface /
/// enum class / enum entry) we are inside; the name hint is set when entering
/// a property binding or a call's argument list.
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
            // class in the TypeScript analyzer, add no nesting depth. A
            // `companion object` is not matched here on purpose: its members
            // are called on the enclosing class, so it adds no segment.
            "class_declaration" | "object_declaration" => {
                let name = child_of_kind(node, "type_identifier").map(|n| self.text(n));
                self.visit_in_scope(name, node);
            }
            // An enum entry with a body scopes its overrides (`E.ADD.apply`),
            // keeping two entries that override the same member distinct.
            "enum_entry" => {
                let name = child_of_kind(node, "simple_identifier").map(|n| self.text(n));
                self.visit_in_scope(name, node);
            }
            // An anonymous `object : Type { … }` scopes its members under the
            // supertype name, keeping them apart from the enclosing type's own
            // members of the same name.
            "object_literal" => {
                let name = self
                    .object_literal_supertype(node)
                    .unwrap_or_else(|| "<object>".to_string());
                self.visit_in_scope(Some(name), node);
            }
            // A `fun` with a body. A bodiless signature (abstract / interface)
            // carries no code and is not recorded; recurse anyway so a lambda
            // in a default parameter value is still reached.
            "function_declaration" => {
                if child_of_kind(node, "function_body").is_some() {
                    let name = child_of_kind(node, "simple_identifier")
                        .map(|n| self.text(n))
                        .unwrap_or_else(|| "<fun>".to_string());
                    self.record_and_descend(name, node);
                } else {
                    self.visit_children(node);
                }
            }
            "secondary_constructor" => {
                self.record_and_descend("<constructor>".to_string(), node);
            }
            // `init { … }` runs at construction time; record it so its churn
            // is attributed (all init blocks of a class share one row).
            "anonymous_initializer" => {
                self.record_and_descend("<init>".to_string(), node);
            }
            "anonymous_function" => {
                self.record_hinted(node, "<anonymous>");
            }
            "lambda_literal" => {
                self.record_hinted(node, "<lambda>");
            }
            "getter" => self.visit_accessor(node, "get"),
            "setter" => self.visit_accessor(node, "set"),
            // `val f = …` / `var f = …` — hint the initializer with the bound
            // name so a lambda held by the property inherits it.
            "property_declaration" => {
                let hint = self.property_name(node);
                self.with_hint(hint, |s| s.visit_children(node));
            }
            // Name a call's function-literal arguments after their callee, e.g.
            // the trailing lambda of `xs.map { … }` becomes `map()`.
            "call_expression" => {
                let mut cursor = node.walk();
                let mut children = node.named_children(&mut cursor);
                let Some(callee_expr) = children.next() else {
                    return;
                };
                let callee = self.callee_name(callee_expr);
                self.visit(callee_expr);
                // A delegate call's lambda is the property's own body
                // (`val cfg by lazy { … }` reads `cfg`), so a pending
                // property-name hint outranks the callee label there.
                let delegated = node
                    .parent()
                    .is_some_and(|p| p.kind() == "property_delegate");
                let hint = if delegated {
                    self.walk.name_hint.clone()
                } else {
                    callee.map(|c| format!("{c}()"))
                };
                self.with_hint(hint, |s| {
                    for child in children {
                        s.visit(child);
                    }
                });
            }
            // Operator-notation calls: `xs forEach { … }` — the literal on
            // the right is named after the infix function, like the dot form.
            "infix_expression" => {
                let mut cursor = node.walk();
                let mut children = node.named_children(&mut cursor);
                match (children.next(), children.next(), children.next()) {
                    (Some(left), Some(op), Some(right)) if op.kind() == "simple_identifier" => {
                        self.visit(left);
                        let hint = format!("{}()", self.text(op));
                        self.with_hint(Some(hint), |s| s.visit(right));
                    }
                    _ => self.visit_children(node),
                }
            }
            _ => self.visit_children(node),
        }
    }
}

impl<'a> SymbolCollector<'a> {
    /// A property accessor with a body is a function of its own, named after
    /// the property it accompanies: `prop.get` / `prop.set`.
    fn visit_accessor(&mut self, node: Node<'a>, accessor: &str) {
        if child_of_kind(node, "function_body").is_none() {
            self.visit_children(node);
            return;
        }
        let name = self
            .accessor_property(node)
            .map(|p| format!("{p}.{accessor}"))
            .unwrap_or_else(|| format!("<{accessor}>"));
        self.record_and_descend(name, node);
    }

    /// The name of the variable a `property_declaration` binds.
    fn property_name(&self, prop: Node) -> Option<String> {
        let vd = child_of_kind(prop, "variable_declaration")?;
        child_of_kind(vd, "simple_identifier").map(|n| self.text(n))
    }

    /// The property a `getter` / `setter` belongs to. A same-line accessor
    /// (`val z: Int get() = 1`) nests inside its property declaration; a
    /// next-line one is parsed as a sibling of it, so also walk back over the
    /// other accessor (and comments) to the nearest property declaration.
    fn accessor_property(&self, node: Node) -> Option<String> {
        if let Some(parent) = node.parent()
            && parent.kind() == "property_declaration"
        {
            return self.property_name(parent);
        }
        let mut prev = node.prev_named_sibling();
        while let Some(p) = prev {
            if p.kind() == "property_declaration" {
                return self.property_name(p);
            }
            if matches!(p.kind(), "getter" | "setter") || p.is_extra() {
                prev = p.prev_named_sibling();
            } else {
                return None;
            }
        }
        None
    }

    /// The supertype an `object : Type { … }` literal implements: `Runnable`
    /// for `object : Runnable`, the base name for `Handler<Int>`, the last
    /// segment for `a.b.Runnable`. `None` for a bare `object { … }`.
    fn object_literal_supertype(&self, node: Node) -> Option<String> {
        let spec = child_of_kind(node, "delegation_specifier")?;
        let ty = child_of_kind(spec, "user_type").or_else(|| {
            child_of_kind(spec, "constructor_invocation")
                .and_then(|ci| child_of_kind(ci, "user_type"))
        })?;
        let mut cursor = ty.walk();
        ty.named_children(&mut cursor)
            .filter(|c| c.kind() == "type_identifier")
            .last()
            .map(|n| self.text(n))
    }

    /// The simple name of a call's callee: `foo` for `foo(...)`, the trailing
    /// navigation segment for member calls like `xs.map(...)`. `None` for
    /// anything else.
    fn callee_name(&self, node: Node) -> Option<String> {
        match node.kind() {
            "simple_identifier" => Some(self.text(node)),
            "navigation_expression" => child_of_kind(node, "navigation_suffix")
                .and_then(|suffix| child_of_kind(suffix, "simple_identifier"))
                .map(|n| self.text(n)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> Vec<String> {
        let mut s = KotlinAnalyzer
            .extract_symbols("test.kt", source)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect::<Vec<_>>();
        s.sort();
        s
    }

    fn symbol_named(source: &str, name: &str) -> Option<Symbol> {
        KotlinAnalyzer
            .extract_symbols("test.kt", source)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
    }

    #[test]
    fn extracts_functions_and_methods() {
        let src = r#"
            fun top(): Int = 1
            class Greeter(val name: String) {
                fun greet(who: String): String = "hi " + who
            }
            object Util {
                fun run() {}
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"top".to_string()));
        assert!(got.contains(&"Greeter.greet".to_string()));
        assert!(got.contains(&"Util.run".to_string()));
    }

    #[test]
    fn companion_object_adds_no_segment() {
        // Companion members are called as `Greeter.create()`, so they are
        // qualified by the class alone.
        let src = r#"
            class Greeter {
                companion object {
                    fun create(): Greeter = Greeter()
                }
            }
        "#;
        assert!(names(src).contains(&"Greeter.create".to_string()));
    }

    #[test]
    fn qualifies_nested_types() {
        let src = r#"
            class Outer {
                class Inner {
                    fun leaf(): Int = 1
                }
            }
        "#;
        assert!(names(src).contains(&"Outer.Inner.leaf".to_string()));
    }

    #[test]
    fn bodiless_signatures_are_not_recorded() {
        // Abstract / interface signatures carry no code, like Go interface
        // methods; default methods with a body are recorded.
        let src = r#"
            interface I {
                fun sig(x: Int): Int
                fun withBody(): Int = 1
            }
        "#;
        let got = names(src);
        assert!(!got.contains(&"I.sig".to_string()));
        assert!(got.contains(&"I.withBody".to_string()));
    }

    #[test]
    fn tracks_function_nesting_depth() {
        let src = r#"
            fun outer(): Int {
                fun inner(): Int {
                    fun deepest(): Int = 1
                    return deepest()
                }
                return inner()
            }
        "#;
        let depth_of = |name: &str| symbol_named(src, name).map(|s| s.depth);
        assert_eq!(depth_of("outer"), Some(1));
        assert_eq!(depth_of("inner"), Some(2));
        assert_eq!(depth_of("deepest"), Some(3));
    }

    #[test]
    fn labels_lambdas_from_bindings_and_callees() {
        let src = r#"
            fun run() {
                val double = { x: Int -> x * 2 }
                val anon = fun(x: Int): Int = x
                listOf(1).map { it + 1 }
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"double".to_string()));
        assert!(got.contains(&"anon".to_string()));
        assert!(got.contains(&"map()".to_string()));
    }

    #[test]
    fn qualifies_lambdas_inside_methods() {
        let src = r#"
            class C {
                fun host() {
                    val cb = { 1 }
                }
            }
        "#;
        // The method is depth 1 (the class adds no level); the lambda it holds
        // is depth 2 and inherits the class qualification.
        assert_eq!(symbol_named(src, "C.host").map(|s| s.depth), Some(1));
        assert_eq!(symbol_named(src, "C.cb").map(|s| s.depth), Some(2));
    }

    #[test]
    fn labels_property_accessors() {
        let src = r#"
            class C {
                val prop: Int
                    get() = 1
                var mut: Int = 0
                    set(value) {
                        field = value
                    }
                val inline: Int get() = 2
            }
        "#;
        // The next-line accessors parse as siblings of their property; the
        // same-line one nests inside it. Both forms carry the property name.
        let got = names(src);
        assert!(got.contains(&"C.prop.get".to_string()));
        assert!(got.contains(&"C.mut.set".to_string()));
        assert!(got.contains(&"C.inline.get".to_string()));
        assert!(!got.contains(&"C.<get>".to_string()));
    }

    #[test]
    fn delegated_property_lambda_keeps_property_name() {
        let src = r#"
            class C {
                val config by lazy {
                    1
                }
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"C.config".to_string()));
        assert!(!got.contains(&"C.lazy()".to_string()));
    }

    #[test]
    fn records_init_blocks() {
        let src = r#"
            class C(n: Int) {
                init {
                    require(n > 0)
                }
            }
        "#;
        let init = symbol_named(src, "C.<init>").unwrap();
        assert_eq!(init.depth, 1);
        assert_eq!((init.start_line, init.end_line), (3, 5));
    }

    #[test]
    fn labels_infix_call_lambdas() {
        let src = r#"
            fun run(xs: List<Int>) {
                xs forEach2 { println(it) }
            }
        "#;
        assert!(names(src).contains(&"forEach2()".to_string()));
    }

    #[test]
    fn scopes_object_literal_members_by_supertype() {
        let src = r#"
            class C {
                fun run() {}
                val r = object : Runnable {
                    override fun run() {}
                }
            }
        "#;
        // The anonymous object's override must not merge with the enclosing
        // class's own `run`.
        let got = names(src);
        assert!(got.contains(&"C.run".to_string()));
        assert!(got.contains(&"C.Runnable.run".to_string()));
    }

    #[test]
    fn secondary_constructor_is_labelled() {
        let src = r#"
            class C(val x: Int) {
                constructor() : this(0)
            }
        "#;
        assert!(names(src).contains(&"C.<constructor>".to_string()));
    }

    #[test]
    fn qualifies_enum_entry_overrides() {
        let src = r#"
            enum class Op {
                ADD {
                    override fun apply(a: Int, b: Int) = a + b
                },
                SUB {
                    override fun apply(a: Int, b: Int) = a - b
                };
                abstract fun apply(a: Int, b: Int): Int
            }
        "#;
        // Each entry scopes its override, so the two overrides stay distinct
        // leaderboard rows. The abstract signature has no body and is not
        // recorded.
        let got = names(src);
        assert!(got.contains(&"Op.ADD.apply".to_string()));
        assert!(got.contains(&"Op.SUB.apply".to_string()));
        assert!(!got.contains(&"Op.apply".to_string()));
    }

    #[test]
    fn extracts_extension_functions() {
        let src = r#"fun String.shout(): String = this + "!""#;
        assert!(names(src).contains(&"shout".to_string()));
    }

    #[test]
    fn records_multi_line_method_range() {
        let src = "class C {\n    fun m(): Int {\n        return 1\n    }\n}\n";
        let m = symbol_named(src, "C.m").unwrap();
        assert_eq!(m.start_line, 2);
        assert_eq!(m.end_line, 4);
    }

    #[test]
    fn invalid_source_is_an_error() {
        assert!(
            KotlinAnalyzer
                .extract_symbols("bad.kt", "class C { fun (")
                .is_err()
        );
    }

    #[test]
    fn empty_and_declaration_only_sources_have_no_symbols() {
        assert!(
            KotlinAnalyzer
                .extract_symbols("empty.kt", "")
                .unwrap()
                .is_empty()
        );
        assert!(
            KotlinAnalyzer
                .extract_symbols("data.kt", "data class P(val x: Int, val y: Int)\n")
                .unwrap()
                .is_empty()
        );
    }
}
