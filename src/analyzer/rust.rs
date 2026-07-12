//! Rust analyzer built on the [`syn`](https://docs.rs/syn) parser.
//!
//! It walks the syntax tree and records every function, method, associated
//! function, and closure together with the source line range it spans. Free
//! functions and methods are named after their identifier; a closure bound to a
//! `let` (`let f = |x| ...;`) inherits that binding's name so the leaderboard
//! stays readable, otherwise it is recorded as `<closure>`.
//!
//! Names are qualified by the enclosing modules, `impl` types, and traits, e.g.
//! `parser.Lexer.next` for a `next` method of `impl Lexer` inside `mod parser`.
//! Entering such a scope qualifies the name but — like a class in the
//! TypeScript analyzer — does not increase nesting depth; only stepping into a
//! function body does.

use anyhow::{Context, Result};
use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    ExprClosure, ImplItemFn, ItemFn, ItemImpl, ItemMod, ItemTrait, Local, Pat, TraitItemFn, Type,
};

use super::{LanguageAnalyzer, Symbol};

pub struct RustAnalyzer;

impl LanguageAnalyzer for RustAnalyzer {
    fn name(&self) -> &'static str {
        "Rust"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn extract_symbols(&self, _path: &str, source: &str) -> Result<Vec<Symbol>> {
        // `syn` refuses to parse invalid Rust; the engine treats an `Err` here as
        // "no dig-down for this file", so a syntactically broken revision simply
        // contributes to the file leaderboard only.
        let file = syn::parse_file(source).context("failed to parse Rust source")?;
        let mut collector = SymbolCollector {
            symbols: Vec::new(),
            scope_stack: Vec::new(),
            name_hint: None,
            depth: 0,
        };
        collector.visit_file(&file);
        Ok(collector.symbols)
    }
}

/// Syntax-tree visitor that accumulates function / method / closure symbols.
struct SymbolCollector {
    symbols: Vec<Symbol>,
    /// Names of the modules, impl types, and traits we are currently inside,
    /// outermost first. Used to qualify recorded names.
    scope_stack: Vec<String>,
    /// Name to attach to the next closure we descend into (set when entering a
    /// `let` binding whose pattern is a plain identifier).
    name_hint: Option<String>,
    /// Number of enclosing function bodies we are currently inside.
    depth: u32,
}

impl SymbolCollector {
    fn record(&mut self, name: String, span: Span) {
        let qualified = if self.scope_stack.is_empty() {
            name
        } else {
            format!("{}.{}", self.scope_stack.join("."), name)
        };
        // `proc-macro2` line/column locations are 1-based, and `end()` lands on
        // the closing token's line, so the range is already inclusive.
        let start_line = span.start().line as u32;
        let end_line = span.end().line as u32;
        self.symbols.push(Symbol {
            name: qualified,
            start_line,
            end_line,
            // The function being recorded sits one level below its enclosers.
            depth: self.depth + 1,
        });
    }

    /// Record `name` for `span`, then walk `body` with the depth bumped by one.
    /// The name hint is cleared for the body so it cannot leak onto a nested
    /// closure, and restored afterwards.
    fn record_and_descend(&mut self, name: String, span: Span, walk: impl FnOnce(&mut Self)) {
        self.record(name, span);
        let saved = self.name_hint.take();
        self.depth += 1;
        walk(self);
        self.depth -= 1;
        self.name_hint = saved;
    }
}

impl<'a> Visit<'a> for SymbolCollector {
    fn visit_item_fn(&mut self, node: &'a ItemFn) {
        let name = node.sig.ident.to_string();
        self.record_and_descend(name, node.span(), |c| visit::visit_item_fn(c, node));
    }

    fn visit_impl_item_fn(&mut self, node: &'a ImplItemFn) {
        let name = node.sig.ident.to_string();
        self.record_and_descend(name, node.span(), |c| visit::visit_impl_item_fn(c, node));
    }

    fn visit_trait_item_fn(&mut self, node: &'a TraitItemFn) {
        // Trait methods with a default body are functions worth tracking; those
        // without one still occupy a signature line and are recorded uniformly.
        let name = node.sig.ident.to_string();
        self.record_and_descend(name, node.span(), |c| visit::visit_trait_item_fn(c, node));
    }

    fn visit_expr_closure(&mut self, node: &'a ExprClosure) {
        let name = self
            .name_hint
            .take()
            .unwrap_or_else(|| "<closure>".to_string());
        self.record_and_descend(name, node.span(), |c| visit::visit_expr_closure(c, node));
    }

    fn visit_item_impl(&mut self, node: &'a ItemImpl) {
        self.scope_stack.push(type_name(&node.self_ty));
        visit::visit_item_impl(self, node);
        self.scope_stack.pop();
    }

    fn visit_item_trait(&mut self, node: &'a ItemTrait) {
        self.scope_stack.push(node.ident.to_string());
        visit::visit_item_trait(self, node);
        self.scope_stack.pop();
    }

    fn visit_item_mod(&mut self, node: &'a ItemMod) {
        self.scope_stack.push(node.ident.to_string());
        visit::visit_item_mod(self, node);
        self.scope_stack.pop();
    }

    fn visit_local(&mut self, node: &'a Local) {
        // `let f = |x| ...;` — hint the closure in the initializer with the
        // binding's name. Restore the previous hint afterwards.
        let saved = self.name_hint.take();
        if let Some(ident) = binding_ident(&node.pat) {
            self.name_hint = Some(ident);
        }
        visit::visit_local(self, node);
        self.name_hint = saved;
    }
}

/// The simple name of an `impl` self-type: `Foo` for `impl Foo`,
/// `impl Trait for Foo`, `impl Foo<T>`, or `impl &Foo`. Falls back to `<type>`
/// for shapes without an obvious name (tuples, slices, ...).
fn type_name(ty: &Type) -> String {
    match ty {
        Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "<type>".to_string()),
        Type::Reference(r) => type_name(&r.elem),
        Type::Group(g) => type_name(&g.elem),
        Type::Paren(p) => type_name(&p.elem),
        _ => "<type>".to_string(),
    }
}

/// The identifier a pattern binds, if it is a plain `x` or a typed `x: T`.
fn binding_ident(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ident(p) => Some(p.ident.to_string()),
        Pat::Type(p) => binding_ident(&p.pat),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> Vec<String> {
        let mut s = RustAnalyzer
            .extract_symbols("test.rs", source)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect::<Vec<_>>();
        s.sort();
        s
    }

    /// The single symbol named `name`, or `None` if it is absent.
    fn symbol_named(source: &str, name: &str) -> Option<Symbol> {
        RustAnalyzer
            .extract_symbols("test.rs", source)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
    }

    #[test]
    fn extracts_functions_methods_and_associated_fns() {
        let src = r#"
            fn top() -> i32 { 1 }
            struct Greeter { name: String }
            impl Greeter {
                fn new(name: String) -> Self { Greeter { name } }
                fn greet(&self) -> String { format!("hi {}", self.name) }
            }
            trait Speak {
                fn speak(&self) -> String { "…".to_string() }
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"top".to_string()));
        assert!(got.contains(&"Greeter.new".to_string()));
        assert!(got.contains(&"Greeter.greet".to_string()));
        assert!(got.contains(&"Speak.speak".to_string()));
    }

    #[test]
    fn qualifies_names_by_module() {
        let src = r#"
            mod parser {
                fn tokenize() {}
                impl Lexer {
                    fn next(&mut self) {}
                }
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"parser.tokenize".to_string()));
        assert!(got.contains(&"parser.Lexer.next".to_string()));
    }

    #[test]
    fn labels_closures_from_let_bindings() {
        let src = r#"
            fn run() {
                let doubler = |x: i32| x * 2;
                [1, 2, 3].iter().map(|n| n + 1);
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"doubler".to_string()));
        // The bare closure passed to `map` has no binding name.
        assert!(got.contains(&"<closure>".to_string()));
    }

    #[test]
    fn tracks_function_nesting_depth() {
        let src = r#"
            fn outer() {
                fn inner() {
                    let deepest = || 1;
                }
            }
            impl C {
                fn method() {
                    fn helper() {}
                }
            }
        "#;
        let syms = RustAnalyzer.extract_symbols("d.rs", src).unwrap();
        let depth_of = |name: &str| syms.iter().find(|s| s.name == name).map(|s| s.depth);
        assert_eq!(depth_of("outer"), Some(1));
        assert_eq!(depth_of("inner"), Some(2));
        assert_eq!(depth_of("deepest"), Some(3));
        // A top-level impl's methods are depth 1; the impl doesn't add a level.
        assert_eq!(depth_of("C.method"), Some(1));
        assert_eq!(depth_of("C.helper"), Some(2));
    }

    #[test]
    fn records_plausible_line_ranges() {
        let src = "fn a() {\n    1\n}\n";
        let syms = RustAnalyzer.extract_symbols("a.rs", src).unwrap();
        let a = syms.iter().find(|s| s.name == "a").unwrap();
        assert_eq!(a.start_line, 1);
        assert_eq!(a.end_line, 3);
    }

    #[test]
    fn invalid_source_is_an_error() {
        assert!(RustAnalyzer.extract_symbols("bad.rs", "fn (").is_err());
    }

    #[test]
    fn trait_impl_qualifies_by_self_type_not_trait() {
        // `impl Display for Point` — the method belongs to `Point`, the concrete
        // type being implemented, not to the `Display` trait.
        let src = r#"
            struct Point;
            impl Display for Point {
                fn fmt(&self, f: &mut Formatter) -> Result { Ok(()) }
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"Point.fmt".to_string()));
        assert!(!got.contains(&"Display.fmt".to_string()));
    }

    #[test]
    fn unwraps_generic_and_reference_self_types() {
        // `impl<T> Wrapper<T>` and `impl Trait for &Thing` should both reduce to
        // the bare type name.
        let src = r#"
            impl<T> Wrapper<T> {
                fn get(&self) -> &T { &self.0 }
            }
            impl Speak for &Thing {
                fn speak(&self) {}
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"Wrapper.get".to_string()));
        assert!(got.contains(&"Thing.speak".to_string()));
    }

    #[test]
    fn labels_closure_from_typed_let_binding() {
        // A `let` with a type annotation (`Pat::Type`) still yields the binding
        // name for the closure it initializes.
        let src = r#"
            fn run() {
                let make: fn() -> i32 = || 1;
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"make".to_string()));
        assert!(!got.contains(&"<closure>".to_string()));
    }

    #[test]
    fn qualifies_by_nested_modules() {
        let src = r#"
            mod a {
                mod b {
                    fn c() {}
                }
            }
        "#;
        assert!(names(src).contains(&"a.b.c".to_string()));
    }

    #[test]
    fn closure_inside_method_is_qualified_and_nested() {
        let src = r#"
            struct S;
            impl S {
                fn run(&self) {
                    let cb = || 1;
                }
            }
        "#;
        // The method sits at depth 1 (impl adds no level); the closure it holds
        // is depth 2 and inherits the impl-type qualification.
        assert_eq!(symbol_named(src, "S.run").map(|s| s.depth), Some(1));
        assert_eq!(symbol_named(src, "S.cb").map(|s| s.depth), Some(2));
    }

    #[test]
    fn records_trait_method_without_default_body() {
        // A signature-only trait method has no body but still occupies a line and
        // is recorded so history that touches the declaration is attributed.
        let src = r#"
            trait Draw {
                fn draw(&self);
            }
        "#;
        assert!(names(src).contains(&"Draw.draw".to_string()));
    }

    #[test]
    fn empty_source_has_no_symbols() {
        assert!(
            RustAnalyzer
                .extract_symbols("empty.rs", "")
                .unwrap()
                .is_empty()
        );
        // A file with only non-function items yields nothing either.
        assert!(
            RustAnalyzer
                .extract_symbols("data.rs", "struct S { x: i32 }\nconst K: i32 = 1;\n")
                .unwrap()
                .is_empty()
        );
    }
}
