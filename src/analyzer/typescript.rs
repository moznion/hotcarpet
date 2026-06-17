//! TypeScript / JavaScript analyzer built on the [oxc](https://oxc.rs) parser.
//!
//! It walks the AST and records every function and method definition together
//! with the source line range it spans. Anonymous functions assigned to a
//! binding (`const f = () => {}`, class fields, object properties) inherit that
//! binding's name so the leaderboard stays readable.

use anyhow::Result;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use oxc_syntax::scope::ScopeFlags;

use super::{LanguageAnalyzer, LineIndex, Symbol};

pub struct TypeScriptAnalyzer;

impl LanguageAnalyzer for TypeScriptAnalyzer {
    fn name(&self) -> &'static str {
        "TypeScript"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"]
    }

    fn extract_symbols(&self, path: &str, source: &str) -> Result<Vec<Symbol>> {
        let allocator = Allocator::default();
        // Pick the dialect from the extension; fall back to TSX, the most
        // permissive superset, for unknown shapes.
        let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::tsx());
        let parsed = Parser::new(&allocator, source, source_type).parse();

        let line_index = LineIndex::new(source);
        let mut collector = SymbolCollector {
            symbols: Vec::new(),
            class_stack: Vec::new(),
            name_hint: None,
            depth: 0,
            line_index: &line_index,
        };
        collector.visit_program(&parsed.program);
        Ok(collector.symbols)
    }
}

/// AST visitor that accumulates function / method symbols.
struct SymbolCollector<'i> {
    symbols: Vec<Symbol>,
    /// Names of the classes we are currently inside, outermost first.
    class_stack: Vec<String>,
    /// Name to attach to the next anonymous function we descend into (set when
    /// entering a binding such as a variable declarator or class field).
    name_hint: Option<String>,
    /// Number of enclosing function bodies we are currently inside.
    depth: u32,
    line_index: &'i LineIndex,
}

impl SymbolCollector<'_> {
    fn record(&mut self, name: String, span: Span) {
        let qualified = if self.class_stack.is_empty() {
            name
        } else {
            format!("{}.{}", self.class_stack.join("."), name)
        };
        let start_line = self.line_index.line_of(span.start);
        // `span.end` is exclusive; step back one byte to land on the last line
        // that actually holds content.
        let end_line = self.line_index.line_of(span.end.saturating_sub(1));
        self.symbols.push(Symbol {
            name: qualified,
            start_line,
            end_line,
            // The function being recorded sits one level below its enclosers.
            depth: self.depth + 1,
        });
    }
}

impl<'a> Visit<'a> for SymbolCollector<'_> {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        let name = it
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_string())
            .or_else(|| self.name_hint.take())
            .unwrap_or_else(|| "<anonymous>".to_string());
        self.record(name, it.span);

        // Don't leak this hint into the function body's nested declarations.
        let saved = self.name_hint.take();
        self.depth += 1;
        walk::walk_function(self, it, flags);
        self.depth -= 1;
        self.name_hint = saved;
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let name = self
            .name_hint
            .take()
            .unwrap_or_else(|| "<arrow>".to_string());
        self.record(name, it.span);
        self.depth += 1;
        walk::walk_arrow_function_expression(self, it);
        self.depth -= 1;
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        let class_name = it
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_string())
            .or_else(|| self.name_hint.take())
            .unwrap_or_else(|| "<anonymous class>".to_string());
        self.class_stack.push(class_name);
        walk::walk_class(self, it);
        self.class_stack.pop();
    }

    fn visit_method_definition(&mut self, it: &MethodDefinition<'a>) {
        // The method's value is a `Function`; hint it with the method name so
        // `visit_function` records it qualified by the enclosing class.
        self.name_hint = it.key.name().map(|n| n.into_owned());
        walk::walk_method_definition(self, it);
        self.name_hint = None;
    }

    fn visit_property_definition(&mut self, it: &PropertyDefinition<'a>) {
        // Class fields may hold an arrow/function: `handler = () => {}`.
        self.name_hint = it.key.name().map(|n| n.into_owned());
        walk::walk_property_definition(self, it);
        self.name_hint = None;
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        let saved = self.name_hint.take();
        if let Some(ident) = it.id.get_identifier_name() {
            self.name_hint = Some(ident.as_str().to_string());
        }
        walk::walk_variable_declarator(self, it);
        self.name_hint = saved;
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        // Name callback arguments after their callee, e.g. an arrow passed to
        // `describe(...)` / `useMemo(...)` is recorded as `describe()`. This is
        // the hint the first function/arrow argument picks up.
        let saved = self.name_hint.take();
        if let Some(callee) = callee_name(&it.callee) {
            self.name_hint = Some(format!("{callee}()"));
        }
        walk::walk_call_expression(self, it);
        self.name_hint = saved;
    }
}

/// The simple name of a call's callee: `foo` for `foo(...)`, the property name
/// for member calls like `arr.map(...)`. `None` for anything more dynamic.
fn callee_name(callee: &Expression) -> Option<String> {
    match callee {
        Expression::Identifier(id) => Some(id.name.as_str().to_string()),
        _ => callee
            .get_member_expr()
            .and_then(|m| m.static_property_name())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> Vec<String> {
        let mut s = TypeScriptAnalyzer
            .extract_symbols("test.ts", source)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect::<Vec<_>>();
        s.sort();
        s
    }

    #[test]
    fn extracts_named_functions_and_methods() {
        let src = r#"
            function top() { return 1; }
            const arrow = () => 2;
            class Greeter {
                constructor() {}
                greet(name: string) { return `hi ${name}`; }
                get value() { return 3; }
                handler = () => 4;
            }
        "#;
        let got = names(src);
        assert!(got.contains(&"top".to_string()));
        assert!(got.contains(&"arrow".to_string()));
        assert!(got.contains(&"Greeter.constructor".to_string()));
        assert!(got.contains(&"Greeter.greet".to_string()));
        assert!(got.contains(&"Greeter.value".to_string()));
        assert!(got.contains(&"Greeter.handler".to_string()));
    }

    #[test]
    fn labels_callback_arguments_with_callee() {
        let src = r#"
            describe("suite", () => {
                it("works", () => {});
            });
            const memo = useMemo(() => compute(), []);
            arr.map((x) => x + 1);
        "#;
        let got = names(src);
        assert!(got.contains(&"describe()".to_string()));
        assert!(got.contains(&"it()".to_string()));
        assert!(got.contains(&"useMemo()".to_string()));
        assert!(got.contains(&"map()".to_string()));
        // No bare "<arrow>" should remain for these named-callee cases.
        assert!(!got.contains(&"<arrow>".to_string()));
    }

    #[test]
    fn tracks_function_nesting_depth() {
        let src = r#"
            function outer() {
                function inner() {
                    const deepest = () => 1;
                }
            }
            class C {
                method() {
                    function helper() {}
                }
            }
        "#;
        let syms = TypeScriptAnalyzer.extract_symbols("d.ts", src).unwrap();
        let depth_of = |name: &str| syms.iter().find(|s| s.name == name).map(|s| s.depth);
        assert_eq!(depth_of("outer"), Some(1));
        assert_eq!(depth_of("inner"), Some(2));
        assert_eq!(depth_of("deepest"), Some(3));
        // A top-level class's methods are depth 1; classes don't add a level.
        assert_eq!(depth_of("C.method"), Some(1));
        assert_eq!(depth_of("C.helper"), Some(2));
    }

    #[test]
    fn records_plausible_line_ranges() {
        let src = "function a() {\n  return 1;\n}\n";
        let syms = TypeScriptAnalyzer.extract_symbols("a.ts", src).unwrap();
        let a = syms.iter().find(|s| s.name == "a").unwrap();
        assert_eq!(a.start_line, 1);
        assert_eq!(a.end_line, 3);
    }
}
