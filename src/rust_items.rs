//! Public items in the Rust port, read with tree-sitter-rust. `done` maps TS
//! exports onto them and `check` confirms recorded symbols still exist.

use anyhow::{Context, Result, anyhow};
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Item {
    /// `name`, or `Type::method` for public methods in inherent impls.
    pub name: String,
    pub kind: &'static str,
    pub line: usize,
}

pub fn parse_items(source: &str) -> Result<Vec<Item>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .context("load Rust grammar")?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter could not parse Rust source"))?;
    let bytes = source.as_bytes();
    let mut items = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        collect(node, bytes, None, &mut items);
    }
    Ok(items)
}

/// Names of `mod x;` / `pub mod x;` declarations in a module file.
pub fn declared_modules(source: &str) -> Result<Vec<String>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .context("load Rust grammar")?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter could not parse Rust source"))?;
    let bytes = source.as_bytes();
    let root = tree.root_node();
    let mut cursor = root.walk();
    Ok(root
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "mod_item")
        .filter_map(|n| n.child_by_field_name("name"))
        .filter_map(|n| n.utf8_text(bytes).ok())
        .map(|s| s.trim_start_matches("r#").to_string())
        .collect())
}

fn is_pub(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|c| c.kind() == "visibility_modifier")
}

fn collect(node: Node<'_>, bytes: &[u8], owner: Option<&str>, out: &mut Vec<Item>) {
    let kind = match node.kind() {
        "function_item" => "fn",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" => "trait",
        "type_item" => "type",
        "const_item" => "const",
        "static_item" => "static",
        "mod_item" => "mod",
        "macro_definition" => "macro",
        "use_declaration" => {
            // `pub use` re-exports a name from this module, the way a TS
            // barrel or `export { a as b }` does; count each exported name.
            if owner.is_none()
                && is_pub(node)
                && let Some(argument) = node.child_by_field_name("argument")
            {
                let mut names = Vec::new();
                use_names(argument, bytes, &mut names);
                for name in names {
                    out.push(Item {
                        name,
                        kind: "use",
                        line: node.start_position().row + 1,
                    });
                }
            }
            return;
        }
        "impl_item" => {
            // Record public methods of inherent impls as `Type::method`.
            if node.child_by_field_name("trait").is_some() {
                return;
            }
            let Some(ty) = node
                .child_by_field_name("type")
                .and_then(|t| t.utf8_text(bytes).ok())
            else {
                return;
            };
            let ty = ty.split('<').next().unwrap_or(ty).trim().to_string();
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    collect(child, bytes, Some(&ty), out);
                }
            }
            return;
        }
        _ => return,
    };
    let exported = is_pub(node) || kind == "macro";
    if !exported {
        return;
    }
    let Some(name) = node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(bytes).ok())
    else {
        return;
    };
    let name = name.trim_start_matches("r#");
    out.push(Item {
        name: match owner {
            Some(owner) => format!("{owner}::{name}"),
            None => name.to_string(),
        },
        kind,
        line: node.start_position().row + 1,
    });
}

/// The names a `use` tree brings into scope: the last path segment, or the
/// `as` alias. Globs and `self` add nothing nameable.
fn use_names(node: Node<'_>, bytes: &[u8], out: &mut Vec<String>) {
    let text = |n: Node<'_>| {
        n.utf8_text(bytes)
            .unwrap_or_default()
            .trim_start_matches("r#")
            .to_string()
    };
    match node.kind() {
        "identifier" | "type_identifier" => out.push(text(node)),
        "scoped_identifier" => {
            if let Some(name) = node.child_by_field_name("name") {
                let name = text(name);
                if name != "self" {
                    out.push(name);
                }
            }
        }
        "use_as_clause" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                out.push(text(alias));
            }
        }
        "scoped_use_list" => {
            if let Some(list) = node.child_by_field_name("list") {
                use_names(list, bytes, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                use_names(child, bytes, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_names_a_pub_use_re_exports() {
        let items = parse_items(
            "pub use super::JsonLineDecoder as WorkerJsonLineDecoder;\n\
             pub use tui_log::{TuiLogFields, TuiLogger};\n\
             pub use a::b::{self, c::D, E as F};\n\
             pub use g::*;\n\
             use hidden::Private;\n",
        )
        .unwrap();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "WorkerJsonLineDecoder",
                "TuiLogFields",
                "TuiLogger",
                "D",
                "F"
            ]
        );
    }

    #[test]
    fn lists_public_items_and_inherent_methods() {
        let items = parse_items(
            r#"
pub struct Store;
struct Hidden;
impl Store {
    pub async fn open() -> Self { Store }
    fn private(&self) {}
}
impl Default for Store { fn default() -> Self { Store } }
pub const APP_ID: &str = "x";
pub(crate) fn helper() {}
"#,
        )
        .unwrap();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Store", "Store::open", "APP_ID", "helper"]);
    }

    #[test]
    fn finds_module_declarations() {
        let mods = declared_modules("pub mod store;\nmod r#type;\nfn x() {}").unwrap();
        assert_eq!(mods, ["store", "type"]);
    }
}
