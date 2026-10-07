//! The import graph of the ported TypeScript and the workspace packages it
//! reaches, condensed into port units.
//!
//! A unit is a strongly connected component: files that import each other
//! (directly or through a cycle) must be ported together, because neither
//! compiles in Rust without the other.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use petgraph::graph::{DiGraph, NodeIndex};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::analyze::{Analyzer, FileAnalysis};
use crate::config::Workspace;
use crate::packages::{Link, Packages, relative_candidates};

pub(crate) const NODE_BUILTINS: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "crypto",
    "dns",
    "events",
    "fs",
    "fs/promises",
    "http",
    "https",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "readline",
    "stream",
    "stream/promises",
    "string_decoder",
    "timers",
    "timers/promises",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "worker_threads",
    "zlib",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    File(String),
    External(String),
    Unresolved,
}

#[derive(Debug)]
pub struct SourceFile {
    pub rel: String,
    pub package: String,
    pub analysis: FileAnalysis,
    /// Graph files this one imports.
    pub deps: BTreeSet<usize>,
    /// The subset of `deps` imported only for types (`import type`). Rust
    /// needs the type to exist, not the module's behavior, so these do not
    /// order the port: the types can be declared ahead of their module.
    pub type_deps: BTreeSet<usize>,
    /// Names imported from each dependency.
    pub imported_names: BTreeMap<usize, BTreeSet<String>>,
    /// npm packages and Node builtins this file imports.
    pub externals: BTreeSet<String>,
    pub unresolved: BTreeSet<String>,
}

#[derive(Debug)]
pub struct TestFile {
    pub rel: String,
    pub analysis: FileAnalysis,
    /// Source files the test needs, including through its test helpers.
    pub deps: BTreeSet<usize>,
    /// Non-test-case helper files (fixtures, harnesses) it pulls in.
    pub helpers: BTreeSet<String>,
    /// Runs against the binary (e2e contract tests), not a module.
    pub binary: bool,
}

#[derive(Debug)]
pub struct Unit {
    pub files: Vec<usize>,
    /// Units this one imports behavior from; they must be ported first.
    pub deps: BTreeSet<usize>,
    /// Units this one imports only types from (and not also behavior).
    pub type_deps: BTreeSet<usize>,
    pub dependents: BTreeSet<usize>,
    /// 0 for units with no in-graph dependencies.
    pub level: usize,
    pub lines: usize,
}

pub struct Graph {
    pub files: Vec<SourceFile>,
    pub by_rel: HashMap<String, usize>,
    pub tests: Vec<TestFile>,
    pub units: Vec<Unit>,
    pub unit_of: Vec<usize>,
    pub packages: Packages,
}

impl Graph {
    pub fn build(ws: &Workspace, analyzer: &Analyzer) -> Result<Self> {
        let packages = Packages::discover(&ws.root, &ws.config.packages_dir)?;
        let exclude = globs(&ws.config.exclude)?;
        let mut state = Builder {
            ws,
            analyzer,
            packages: &packages,
            exclude: &exclude,
            analyses: HashMap::new(),
        };

        // Seed with every non-test source file under the roots.
        let mut seeds = Vec::new();
        for root in &ws.config.roots {
            for rel in ts_files(ws, root) {
                if !ws.is_test(&rel) && !exclude.is_match(&rel) {
                    seeds.push(rel);
                }
            }
        }
        let mut sources = state.close_over(seeds)?;

        // Tests: every test-marked file in the primary package, plus tests
        // of other packages whose sources are all already in the graph.
        let primary = packages
            .iter()
            .find(|p| p.name == ws.config.primary_package)
            .map(|p| p.dir.clone())
            .context("primary_package is not a workspace package")?;
        let involved: BTreeSet<String> = sources
            .iter()
            .filter_map(|rel| packages.owner(rel).map(|p| p.dir.clone()))
            .collect();
        let mut test_rels = Vec::new();
        for dir in &involved {
            for rel in ts_files(ws, dir) {
                if ws.is_test(&rel) && !exclude.is_match(&rel) {
                    test_rels.push(rel);
                }
            }
        }
        let parsed: Vec<(String, FileAnalysis)> = test_rels
            .par_iter()
            .filter_map(|rel| state.parse(rel).ok().map(|a| (rel.clone(), a)))
            .collect();
        let test_analyses: HashMap<String, FileAnalysis> = parsed.into_iter().collect();

        // Sources reached only from primary-package tests still need porting
        // for those tests to run.
        let mut extra = Vec::new();
        for (rel, analysis) in &test_analyses {
            if !rel.starts_with(&format!("{primary}/")) {
                continue;
            }
            for import in &analysis.imports {
                if let Resolved::File(dep) = state.resolve(rel, &import.specifier)
                    && !ws.is_test(&dep)
                    && state.in_scope(&dep)
                    && !exclude.is_match(&dep)
                    && !sources.contains(&dep)
                {
                    extra.push(dep);
                }
            }
        }
        sources.extend(state.close_over(extra)?);

        // Assemble source nodes.
        let mut rels: Vec<String> = sources.into_iter().collect();
        rels.sort();
        let by_rel: HashMap<String, usize> = rels
            .iter()
            .enumerate()
            .map(|(i, r)| (r.clone(), i))
            .collect();
        let mut files = Vec::new();
        for rel in &rels {
            let analysis = state.analyses.remove(rel).expect("analysed during closure");
            let mut deps = BTreeSet::new();
            let mut value_deps = BTreeSet::new();
            let mut imported_names: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
            let mut externals = BTreeSet::new();
            let mut unresolved = BTreeSet::new();
            for import in &analysis.imports {
                match state.resolve(rel, &import.specifier) {
                    Resolved::File(dep) => {
                        if let Some(&i) = by_rel.get(&dep) {
                            deps.insert(i);
                            imported_names
                                .entry(i)
                                .or_default()
                                .extend(import.names.iter().cloned());
                            if import.kind != crate::analyze::ImportKind::Type {
                                value_deps.insert(i);
                            }
                        }
                    }
                    Resolved::External(name) => {
                        externals.insert(name);
                    }
                    Resolved::Unresolved => {
                        unresolved.insert(import.specifier.clone());
                    }
                }
            }
            let type_deps = deps.difference(&value_deps).copied().collect();
            files.push(SourceFile {
                rel: rel.clone(),
                package: packages
                    .owner(rel)
                    .map(|p| p.name.clone())
                    .unwrap_or_default(),
                analysis,
                deps,
                type_deps,
                imported_names,
                externals,
                unresolved,
            });
        }

        let tests = assemble_tests(ws, &state, &test_analyses, &by_rel, &primary);
        let (units, unit_of) = condense(&files);
        Ok(Self {
            files,
            by_rel,
            tests,
            units,
            unit_of,
            packages,
        })
    }

    pub fn total_lines(&self) -> usize {
        self.files.iter().map(|f| f.analysis.lines).sum()
    }
}

struct Builder<'a> {
    ws: &'a Workspace,
    analyzer: &'a Analyzer,
    packages: &'a Packages,
    exclude: &'a GlobSet,
    analyses: HashMap<String, FileAnalysis>,
}

impl Builder<'_> {
    fn parse(&self, rel: &str) -> Result<FileAnalysis> {
        let source = std::fs::read_to_string(self.ws.root.join(rel))
            .with_context(|| format!("read {rel}"))?;
        self.analyzer.analyze(rel, &source)
    }

    fn exists(&self, rel: &str) -> bool {
        self.ws.root.join(rel).is_file()
    }

    fn first_existing(&self, candidates: Vec<String>) -> Option<String> {
        candidates
            .into_iter()
            .find(|c| self.ws.is_source(c) && self.exists(c))
    }

    fn resolve(&self, from: &str, specifier: &str) -> Resolved {
        if specifier.ends_with(".json") {
            // Data files (package.json, generated catalogs) are embedded,
            // not ported: see the `json-asset` mapping.
            return Resolved::External("json-asset".into());
        }
        if specifier.starts_with('.') {
            let dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            return match self.first_existing(relative_candidates(dir, specifier)) {
                Some(rel) => Resolved::File(rel),
                None => Resolved::Unresolved,
            };
        }
        if let Some(builtin) = specifier.strip_prefix("node:") {
            return Resolved::External(format!("node:{builtin}"));
        }
        if NODE_BUILTINS.contains(&specifier) {
            return Resolved::External(format!("node:{specifier}"));
        }
        match self.packages.link(self.packages.owner(from), specifier) {
            Link::Workspace(package, _)
                if self.ws.config.external_packages.contains(&package.name) =>
            {
                Resolved::External(package.name.clone())
            }
            Link::Workspace(package, subpath) => {
                match self.first_existing(self.packages.candidates(package, &subpath)) {
                    Some(rel) => Resolved::File(rel),
                    None => Resolved::Unresolved,
                }
            }
            Link::Npm(name) => Resolved::External(name),
        }
    }

    /// Only files inside a workspace package join the graph; anything else
    /// (server code a test borrows) stays outside the port.
    fn in_scope(&self, rel: &str) -> bool {
        self.packages
            .owner(rel)
            .is_some_and(|p| !self.ws.config.external_packages.contains(&p.name))
    }

    /// Parse `seeds` and every non-test source file they reach.
    fn close_over(&mut self, seeds: Vec<String>) -> Result<BTreeSet<String>> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = seeds.into_iter().collect();
        while !queue.is_empty() {
            let layer: Vec<String> = queue
                .drain(..)
                .filter(|r| !self.analyses.contains_key(r) && seen.insert(r.clone()))
                .collect();
            let parsed: Vec<(String, Result<FileAnalysis>)> = layer
                .par_iter()
                .map(|r| (r.clone(), self.parse(r)))
                .collect();
            for (rel, analysis) in parsed {
                let analysis = analysis?;
                for import in &analysis.imports {
                    if let Resolved::File(dep) = self.resolve(&rel, &import.specifier)
                        && !self.ws.is_test(&dep)
                        && self.in_scope(&dep)
                        && !self.exclude.is_match(&dep)
                        && !self.analyses.contains_key(&dep)
                        && !seen.contains(&dep)
                    {
                        queue.push_back(dep);
                    }
                }
                self.analyses.insert(rel, analysis);
            }
        }
        // Include files analysed by an earlier closure pass.
        Ok(self.analyses.keys().cloned().chain(seen).collect())
    }
}

fn assemble_tests(
    ws: &Workspace,
    state: &Builder<'_>,
    analyses: &HashMap<String, FileAnalysis>,
    by_rel: &HashMap<String, usize>,
    primary: &str,
) -> Vec<TestFile> {
    // Direct edges for every test-marked file (cases and helpers alike).
    let mut direct: HashMap<&str, (BTreeSet<usize>, BTreeSet<String>, bool)> = HashMap::new();
    for (rel, analysis) in analyses {
        let mut deps = BTreeSet::new();
        let mut helpers = BTreeSet::new();
        let mut outside = false;
        // Type-only imports count too: the Rust test needs the Rust type.
        for import in &analysis.imports {
            match state.resolve(rel, &import.specifier) {
                Resolved::File(dep) if ws.is_test(&dep) => {
                    helpers.insert(dep);
                }
                Resolved::File(dep) => match by_rel.get(&dep) {
                    Some(&i) => {
                        deps.insert(i);
                    }
                    None => outside = true,
                },
                _ => {}
            }
        }
        direct.insert(rel.as_str(), (deps, helpers, outside));
    }

    let mut tests = Vec::new();
    for rel in analyses.keys() {
        if !is_test_case(rel) {
            continue;
        }
        // Fold helper dependencies transitively.
        let mut deps = BTreeSet::new();
        let mut helpers = BTreeSet::new();
        let mut outside = false;
        let mut stack = vec![rel.clone()];
        let mut visited = BTreeSet::new();
        while let Some(current) = stack.pop() {
            if !visited.insert(current.clone()) {
                continue;
            }
            let Some((d, h, o)) = direct.get(current.as_str()) else {
                continue;
            };
            deps.extend(d.iter().copied());
            outside |= *o;
            for helper in h {
                if helper != rel && !is_test_case(helper) {
                    helpers.insert(helper.clone());
                }
                stack.push(helper.clone());
            }
        }
        // Other packages' tests only count when they stay inside the graph.
        if outside && !rel.starts_with(&format!("{primary}/")) {
            continue;
        }
        if deps.is_empty() {
            continue;
        }
        tests.push(TestFile {
            rel: rel.clone(),
            analysis: analyses[rel].clone(),
            deps,
            helpers,
            binary: ws.is_binary_test(rel),
        });
    }
    tests.sort_by(|a, b| a.rel.cmp(&b.rel));
    tests
}

pub(crate) fn is_test_case(rel: &str) -> bool {
    rel.contains(".test.") || rel.contains(".spec.")
}

/// Strongly connected components over value imports, in dependency-first
/// order, with levels. Type-only imports are left out: in a real CLI this
/// was built on, counting them turned its agent loop into one 100-file
/// cycle, while value imports alone left no cycle larger than five files.
fn condense(files: &[SourceFile]) -> (Vec<Unit>, Vec<usize>) {
    let mut graph = DiGraph::<usize, ()>::new();
    let nodes: Vec<NodeIndex> = (0..files.len()).map(|i| graph.add_node(i)).collect();
    for (i, file) in files.iter().enumerate() {
        for dep in file.deps.difference(&file.type_deps) {
            graph.add_edge(nodes[i], nodes[*dep], ());
        }
    }
    // Postorder over importer → dependency edges yields dependencies first.
    let sccs = petgraph::algo::tarjan_scc(&graph);
    let mut unit_of = vec![0; files.len()];
    let mut units: Vec<Unit> = Vec::with_capacity(sccs.len());
    for (u, scc) in sccs.iter().enumerate() {
        let mut members: Vec<usize> = scc.iter().map(|n| graph[*n]).collect();
        members.sort();
        for &f in &members {
            unit_of[f] = u;
        }
        units.push(Unit {
            lines: members.iter().map(|&f| files[f].analysis.lines).sum(),
            files: members,
            deps: BTreeSet::new(),
            type_deps: BTreeSet::new(),
            dependents: BTreeSet::new(),
            level: 0,
        });
    }
    for u in 0..units.len() {
        let deps: BTreeSet<usize> = units[u]
            .files
            .iter()
            .flat_map(|&f| {
                files[f]
                    .deps
                    .difference(&files[f].type_deps)
                    .map(|&d| unit_of[d])
            })
            .filter(|&d| d != u)
            .collect();
        let type_deps: BTreeSet<usize> = units[u]
            .files
            .iter()
            .flat_map(|&f| files[f].type_deps.iter().map(|&d| unit_of[d]))
            .filter(|&d| d != u && !deps.contains(&d))
            .collect();
        // tarjan_scc emits dependencies before dependents; levels rely on it.
        debug_assert!(
            deps.iter().all(|&d| d < u),
            "unit order is not dependency-first"
        );
        for &d in &deps {
            units[d].dependents.insert(u);
        }
        units[u].level = deps.iter().map(|&d| units[d].level + 1).max().unwrap_or(0);
        units[u].deps = deps;
        units[u].type_deps = type_deps;
    }
    (units, unit_of)
}

pub(crate) fn ts_files(ws: &Workspace, dir: &str) -> Vec<String> {
    let base = ws.root.join(dir);
    WalkDir::new(&base)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(name.as_ref(), "node_modules" | "dist" | ".git" | "target")
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(&ws.root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .filter(|rel| ws.is_source(rel))
        .collect()
}

fn globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p).with_context(|| format!("bad exclude glob {p}"))?);
    }
    Ok(builder.build()?)
}

/// Where a TypeScript file lands: `(file path relative to its crate's src/,
/// module path)`. Modules listed in `components.toml` land in the UI
/// library crate; everything else mirrors the TS tree in the primary crate.
/// `Workspace::crate_src(module)` finds the crate from the module path.
pub fn rust_target(ws: &Workspace, packages: &Packages, rel: &str) -> (String, String) {
    if let Some(ui) = &ws.ui
        && let Some(m) = ui.module_for(rel)
    {
        return (
            m.rust.clone(),
            module_for_file(&ui.library.crate_name, &m.rust),
        );
    }
    let package = packages.owner(rel);
    let (prefix, within) = match package {
        Some(p) if p.name == ws.config.primary_package => (
            Vec::new(),
            rel.strip_prefix(&format!("{}/", p.dir)).unwrap_or(rel),
        ),
        Some(p) => {
            let short = p.name.rsplit('/').next().unwrap_or(&p.name);
            (
                vec![snake(short)],
                rel.strip_prefix(&format!("{}/", p.dir)).unwrap_or(rel),
            )
        }
        None => (Vec::new(), rel),
    };
    let within = within.strip_prefix("src/").unwrap_or(within);
    let stem = within
        .strip_suffix(".tsx")
        .or_else(|| within.strip_suffix(".ts"))
        .or_else(|| within.strip_suffix(".mjs"))
        .or_else(|| within.strip_suffix(".cjs"))
        .or_else(|| within.strip_suffix(".jsx"))
        .or_else(|| within.strip_suffix(".js"))
        .unwrap_or(within);
    let mut segments: Vec<String> = prefix;
    segments.extend(stem.split('/').map(snake));
    let is_index = segments.last().is_some_and(|s| s == "index");
    if is_index {
        segments.pop();
    }
    let module = std::iter::once(ws.config.rust_crate_name.clone())
        .chain(segments.iter().cloned())
        .collect::<Vec<_>>()
        .join("::");
    let file = if segments.is_empty() {
        "lib.rs".to_string()
    } else if is_index {
        format!("{}/mod.rs", segments.join("/"))
    } else {
        format!("{}.rs", segments.join("/"))
    };
    (file, module)
}

/// Module path of `rust_file` (relative to `crate_name`'s `src/`).
pub fn module_for_file(crate_name: &str, rust_file: &str) -> String {
    let stem = rust_file.strip_suffix(".rs").unwrap_or(rust_file);
    let stem = stem.strip_suffix("/mod").unwrap_or(stem);
    if stem == "lib" || stem == "main" {
        return crate_name.to_string();
    }
    std::iter::once(crate_name)
        .chain(stem.split('/'))
        .collect::<Vec<_>>()
        .join("::")
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "yield",
];

/// `cli-client` → `cli_client`, `DevelopApp` → `develop_app`, keywords get a
/// trailing underscore.
pub fn snake(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '-' || c == '.' || c == ' ' {
            out.push('_');
        } else if c.is_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if i > 0 && (prev_lower || next_lower) && !out.ends_with('_') {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    if RUST_KEYWORDS.contains(&out.as_str()) {
        out.push('_');
    }
    out
}

/// Group files by package for summaries.
pub fn by_package(graph: &Graph) -> BTreeMap<String, Vec<usize>> {
    let mut out: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, f) in graph.files.iter().enumerate() {
        out.entry(f.package.clone()).or_default().push(i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::snake;

    #[test]
    fn snake_cases_ts_names() {
        assert_eq!(snake("cli-client"), "cli_client");
        assert_eq!(snake("DevelopApp"), "develop_app");
        assert_eq!(snake("parseJSONBody"), "parse_json_body");
        assert_eq!(snake("type"), "type_");
        assert_eq!(snake("index.test"), "index_test");
    }
}
