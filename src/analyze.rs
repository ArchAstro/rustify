//! One tree-sitter pass per TypeScript file: imports, exports, and the
//! construct rules from `mappings.toml` that match.

use anyhow::{Context, Result, anyhow};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

use crate::mappings::Mappings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    Value,
    Type,
    Dynamic,
    Require,
    Reexport,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Import {
    pub specifier: String,
    pub kind: ImportKind,
    pub line: usize,
    /// Named bindings (`{ a, b as c }` records `a`, `b`); empty for
    /// namespace, default-only, and side-effect imports.
    pub names: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Export {
    pub name: String,
    pub kind: &'static str,
    pub line: usize,
    pub is_async: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    pub rule: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FileAnalysis {
    pub lines: usize,
    pub imports: Vec<Import>,
    pub exports: Vec<Export>,
    pub hits: Vec<Hit>,
}

/// Compiled construct queries for one grammar.
struct Compiled {
    language: Language,
    queries: Vec<CompiledRule>,
}

struct CompiledRule {
    id: String,
    query: Query,
    capture: u32,
    contains: Option<String>,
}

pub struct Analyzer {
    ts: Compiled,
    tsx: Compiled,
}

impl Analyzer {
    pub fn new(mappings: &Mappings) -> Result<Self> {
        Ok(Self {
            ts: compile(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                mappings,
                false,
            )?,
            tsx: compile(tree_sitter_typescript::LANGUAGE_TSX.into(), mappings, true)?,
        })
    }

    pub fn analyze(&self, path: &str, source: &str) -> Result<FileAnalysis> {
        let compiled = if path.ends_with(".tsx") {
            &self.tsx
        } else {
            &self.ts
        };
        let mut parser = Parser::new();
        parser
            .set_language(&compiled.language)
            .context("load TypeScript grammar")?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow!("tree-sitter could not parse {path}"))?;
        let root = tree.root_node();
        let bytes = source.as_bytes();

        let mut imports = Vec::new();
        let mut exports = Vec::new();
        walk(root, &mut |node| {
            collect_import(node, bytes, &mut imports);
            if node.kind() == "export_statement" && node.child_by_field_name("source").is_none() {
                collect_exports(node, bytes, &mut exports);
            }
        });

        let mut hits = Vec::new();
        for rule in &compiled.queries {
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&rule.query, root, bytes);
            let mut seen = std::collections::BTreeSet::new();
            while let Some(m) = matches.next() {
                let node = m
                    .captures
                    .iter()
                    .find(|c| c.index == rule.capture)
                    .or_else(|| m.captures.first())
                    .map(|c| c.node);
                let Some(mut node) = node else { continue };
                if let Some(kind) = &rule.contains {
                    match find_inside(node, kind) {
                        Some(inner) => node = inner,
                        None => continue,
                    }
                }
                let line = node.start_position().row + 1;
                if !seen.insert(line) {
                    continue;
                }
                hits.push(Hit {
                    rule: rule.id.clone(),
                    line,
                    snippet: snippet(source, line),
                });
            }
        }
        hits.sort_by_key(|h| h.line);

        Ok(FileAnalysis {
            lines: source.lines().count(),
            imports,
            exports,
            hits,
        })
    }
}

fn compile(language: Language, mappings: &Mappings, tsx: bool) -> Result<Compiled> {
    let mut queries = Vec::new();
    for rule in &mappings.constructs {
        if rule.tsx_only && !tsx {
            continue;
        }
        let query = Query::new(&language, &rule.query)
            .map_err(|e| anyhow!("construct `{}` query does not compile: {e}", rule.id))?;
        let capture = query
            .capture_names()
            .iter()
            .position(|n| *n == "match")
            .unwrap_or(0) as u32;
        queries.push(CompiledRule {
            id: rule.id.clone(),
            query,
            capture,
            contains: rule.contains.clone(),
        });
    }
    Ok(Compiled { language, queries })
}

/// First node of `kind` inside `node`, not descending into nested functions
/// (an await in a callback defined in a loop does not make the loop wait).
fn find_inside<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
        if matches!(
            child.kind(),
            "arrow_function" | "function_expression" | "function_declaration" | "method_definition"
        ) {
            continue;
        }
        if let Some(found) = find_inside(child, kind) {
            return Some(found);
        }
    }
    None
}

fn walk<'t>(node: Node<'t>, visit: &mut impl FnMut(Node<'t>)) {
    visit(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, visit);
    }
}

fn text<'a>(node: Node<'_>, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

fn string_value(node: Node<'_>, bytes: &[u8]) -> String {
    text(node, bytes)
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

fn has_token(node: Node<'_>, token: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| c.kind() == token)
}

/// Whether `node` sits inside a TypeScript type (a `typeof import(...)`
/// query, a type annotation, alias, or type arguments) rather than in
/// runtime code.
fn in_type_position(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "type_query"
            | "type_annotation"
            | "type_alias_declaration"
            | "type_arguments"
            | "interface_declaration"
            | "generic_type"
            | "lookup_type"
            | "nested_type_identifier" => return true,
            "statement_block"
            | "program"
            | "class_body"
            | "arrow_function"
            | "function_declaration"
            | "method_definition" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

fn collect_import(node: Node<'_>, bytes: &[u8], out: &mut Vec<Import>) {
    let line = node.start_position().row + 1;
    match node.kind() {
        "import_statement" => {
            let Some(source) = node.child_by_field_name("source") else {
                return;
            };
            let names = specifier_names(node, bytes, "import_specifier");
            let kind = if has_token(node, "type") || all_specifiers_typed(node, "import_specifier")
            {
                ImportKind::Type
            } else {
                ImportKind::Value
            };
            out.push(Import {
                specifier: string_value(source, bytes),
                kind,
                line,
                names,
            });
        }
        "export_statement" => {
            if let Some(source) = node.child_by_field_name("source") {
                let kind =
                    if has_token(node, "type") || all_specifiers_typed(node, "export_specifier") {
                        ImportKind::Type
                    } else {
                        ImportKind::Reexport
                    };
                out.push(Import {
                    specifier: string_value(source, bytes),
                    kind,
                    line,
                    names: specifier_names(node, bytes, "export_specifier"),
                });
            }
        }
        "call_expression" => {
            let Some(function) = node.child_by_field_name("function") else {
                return;
            };
            let kind = match function.kind() {
                // `typeof import("pkg").Name` in a type position is a type
                // query, not a runtime load: it must not make the module a
                // value dependency.
                "import" if in_type_position(node) => ImportKind::Type,
                "import" => ImportKind::Dynamic,
                "identifier" if text(function, bytes) == "require" => ImportKind::Require,
                _ => return,
            };
            let Some(arguments) = node.child_by_field_name("arguments") else {
                return;
            };
            if let Some(first) = arguments.named_child(0)
                && first.kind() == "string"
            {
                out.push(Import {
                    specifier: string_value(first, bytes),
                    kind,
                    line,
                    names: Vec::new(),
                });
            }
        }
        _ => {}
    }
}

/// `import { type A, type B } from` — every named binding is type-only (and
/// there is no default or namespace binding).
fn all_specifiers_typed(node: Node<'_>, kind: &str) -> bool {
    let mut specifiers = 0;
    let mut typed = 0;
    let mut other_bindings = false;
    walk(node, &mut |n| {
        if n.kind() == kind {
            specifiers += 1;
            if has_token(n, "type") {
                typed += 1;
            }
        }
        if matches!(n.kind(), "namespace_import" | "namespace_export") {
            other_bindings = true;
        }
    });
    if let Some(clause) = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() == "import_clause")
        && clause
            .named_children(&mut clause.walk())
            .any(|c| c.kind() == "identifier")
    {
        other_bindings = true;
    }
    specifiers > 0 && typed == specifiers && !other_bindings
}

fn specifier_names(node: Node<'_>, bytes: &[u8], kind: &str) -> Vec<String> {
    let mut names = Vec::new();
    walk(node, &mut |n| {
        if n.kind() == kind
            && let Some(name) = n.child_by_field_name("name")
        {
            names.push(text(name, bytes).to_string());
        }
    });
    names
}

fn collect_exports(node: Node<'_>, bytes: &[u8], out: &mut Vec<Export>) {
    let line = node.start_position().row + 1;
    if let Some(declaration) = node.child_by_field_name("declaration") {
        let kind = match declaration.kind() {
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                "function"
            }
            "class_declaration" | "abstract_class_declaration" => "class",
            "interface_declaration" => "interface",
            "type_alias_declaration" => "type",
            "enum_declaration" => "enum",
            "lexical_declaration" | "variable_declaration" => "const",
            "internal_module" | "module" => "namespace",
            _ => "other",
        };
        if kind == "const" {
            let mut cursor = declaration.walk();
            for declarator in declaration.named_children(&mut cursor) {
                if declarator.kind() != "variable_declarator" {
                    continue;
                }
                let Some(name) = declarator.child_by_field_name("name") else {
                    continue;
                };
                let value = declarator.child_by_field_name("value");
                let is_fn = value.is_some_and(|v| {
                    matches!(
                        v.kind(),
                        "arrow_function" | "function_expression" | "function"
                    )
                });
                out.push(Export {
                    name: text(name, bytes).to_string(),
                    kind: if is_fn { "function" } else { "const" },
                    line,
                    is_async: value.is_some_and(|v| is_fn && has_token(v, "async")),
                });
            }
        } else if let Some(name) = declaration.child_by_field_name("name") {
            out.push(Export {
                name: text(name, bytes).to_string(),
                kind,
                line,
                is_async: has_token(declaration, "async"),
            });
        }
        return;
    }
    if has_token(node, "default") {
        out.push(Export {
            name: "default".into(),
            kind: "default",
            line,
            is_async: false,
        });
        return;
    }
    walk(node, &mut |n| {
        if n.kind() == "export_specifier" {
            let alias = n
                .child_by_field_name("alias")
                .or(n.child_by_field_name("name"));
            if let Some(alias) = alias {
                out.push(Export {
                    name: text(alias, bytes).to_string(),
                    kind: "reexport",
                    line,
                    is_async: false,
                });
            }
        }
    });
}

fn snippet(source: &str, line: usize) -> String {
    let raw = source.lines().nth(line - 1).unwrap_or("").trim();
    let mut out: String = raw.chars().take(110).collect();
    if raw.chars().count() > 110 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyzer() -> Analyzer {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/typescript/mappings.toml"
        );
        let mappings: Mappings = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        Analyzer::new(&mappings).unwrap()
    }

    fn kinds(source: &str) -> Vec<(String, ImportKind)> {
        analyzer()
            .analyze("x.ts", source)
            .unwrap()
            .imports
            .into_iter()
            .map(|i| (i.specifier, i.kind))
            .collect()
    }

    #[test]
    fn a_typeof_import_type_query_is_a_type_dependency() {
        let got = kinds(
            "type Loader = () => Promise<typeof import(\"@acme/tui\").ModelProviders>;\n\
             let x: typeof import(\"./a\") | undefined;\n",
        );
        assert_eq!(
            got,
            vec![
                ("@acme/tui".to_owned(), ImportKind::Type),
                ("./a".to_owned(), ImportKind::Type),
            ]
        );
    }

    #[test]
    fn a_runtime_dynamic_import_stays_dynamic() {
        let got = kinds("async function load() { return await import(\"./b\"); }\n");
        assert_eq!(got, vec![("./b".to_owned(), ImportKind::Dynamic)]);
    }
}
