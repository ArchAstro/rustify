//! `mappings.toml`: how TypeScript constructs and npm packages map to
//! Rust. Kept as data so the guidance improves without touching the harness.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::Workspace;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mappings {
    #[serde(default, rename = "construct")]
    pub constructs: Vec<ConstructRule>,
    #[serde(default, rename = "package")]
    pub packages: Vec<PackageRule>,
}

/// A TypeScript construct found with a tree-sitter query.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConstructRule {
    pub id: String,
    pub title: String,
    /// tree-sitter-typescript query. The capture named `@match` marks the
    /// reported node; without one, the whole pattern's first capture is used.
    pub query: String,
    /// The Rust abstraction to use.
    pub rust: String,
    #[serde(default)]
    pub hazards: Vec<String>,
    /// The query uses JSX nodes, which only the TSX grammar has.
    #[serde(default)]
    pub tsx_only: bool,
    /// Only report `@match` when this node kind occurs somewhere inside it
    /// (not counting nested functions); the hit is reported at that inner
    /// node. Queries cannot express "at any depth", so loops that await use
    /// this.
    #[serde(default)]
    pub contains: Option<String>,
}

/// An import that leaves the graph: an npm package or a Node builtin.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRule {
    /// npm name (`commander`, `@acme/sdk`) or builtin (`node:fs`).
    pub npm: String,
    pub rust: String,
    #[serde(default)]
    pub crates: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

impl Mappings {
    pub fn load(ws: &Workspace) -> Result<Self> {
        let path = ws.port_file("mappings.toml");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn package_index(&self) -> BTreeMap<&str, &PackageRule> {
        self.packages.iter().map(|p| (p.npm.as_str(), p)).collect()
    }

    /// Rule for an external import, trying the exact name, then `node:x` for
    /// a bare builtin, then the package root of a deep import.
    pub fn package_for<'a>(&'a self, name: &str) -> Option<&'a PackageRule> {
        let index = self.package_index();
        if let Some(rule) = index.get(name) {
            return Some(rule);
        }
        index.get(format!("node:{name}").as_str()).copied()
    }
}
