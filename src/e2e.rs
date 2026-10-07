//! `rustify e2e`: which tests exercise the program through its process
//! boundary (arguments, output, exit code, files) without importing its
//! source. Those tests can run unchanged against the Rust binary, so they
//! are the port's acceptance suite; a test that imports source has to be
//! ported with its module instead.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde_json::json;

use crate::analyze::{Analyzer, FileAnalysis, ImportKind};
use crate::config::Workspace;
use crate::graph::{Graph, NODE_BUILTINS, is_test_case, ts_files};
use crate::packages::{Link, relative_candidates, split_name};

/// Modules a test imports to start a process.
const SPAWNERS: &[&str] = &[
    "child_process",
    "execa",
    "tinyexec",
    "nano-spawn",
    "cross-spawn",
    "zx",
    "node-pty",
    "@lydell/node-pty",
];

/// Runtime globals that start a process without an import.
const GLOBAL_SPAWNS: &[&str] = &["Bun.spawn", "Bun.$", "Deno.Command"];

/// Imports every test has; not worth listing as something a test needs.
const RUNNERS: &[&str] = &[
    "vitest",
    "jest",
    "@jest/globals",
    "mocha",
    "chai",
    "ava",
    "tap",
    "bun:test",
    "test",
];

#[derive(Default)]
struct Reach {
    spawns: bool,
    /// Source files (and workspace packages) reached directly or through
    /// test helpers.
    sources: BTreeSet<String>,
    /// npm packages other than the test runner and the process spawner.
    needs: BTreeSet<String>,
}

pub fn run(ws: &Workspace, graph: &Graph, analyzer: &Analyzer, json: bool) -> Result<bool> {
    let analyses = test_analyses(ws, graph, analyzer);
    let mut black_box = Vec::new();
    let mut mixed = Vec::new();
    let mut in_process = 0usize;
    for rel in analyses.keys().filter(|rel| is_test_case(rel)) {
        let reach = reach(ws, graph, &analyses, rel);
        if !reach.spawns {
            in_process += 1;
        } else if reach.sources.is_empty() {
            black_box.push((rel, reach));
        } else {
            mixed.push((rel, reach));
        }
    }
    let unmarked: Vec<&String> = black_box
        .iter()
        .map(|(rel, _)| *rel)
        .filter(|rel| !ws.is_binary_test(rel))
        .collect();

    if json {
        let list = |tests: &[(&String, Reach)]| -> Vec<serde_json::Value> {
            tests
                .iter()
                .map(|(rel, reach)| {
                    json!({
                        "ts": rel,
                        "lines": analyses[*rel].lines,
                        "binary_marked": ws.is_binary_test(rel),
                        "imports_source": reach.sources,
                        "needs": reach.needs,
                    })
                })
                .collect()
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "black_box": list(&black_box),
                "mixed": list(&mixed),
                "in_process": in_process,
            }))?
        );
        return Ok(true);
    }

    println!(
        "{} test file(s): {} run the program as a process without importing its source, {} run it but also import source, {} never start a process",
        black_box.len() + mixed.len() + in_process,
        black_box.len(),
        mixed.len(),
        in_process
    );
    if !black_box.is_empty() {
        println!(
            "\nProcess tests with no source imports (can run against the Rust binary as they are):"
        );
        for (rel, reach) in &black_box {
            let needs = if reach.needs.is_empty() {
                String::new()
            } else {
                let names: Vec<&str> = reach.needs.iter().map(String::as_str).collect();
                format!("  needs: {}", names.join(", "))
            };
            println!("  {rel}  ({} lines){needs}", analyses[*rel].lines);
        }
    }
    if !mixed.is_empty() {
        println!(
            "\nProcess tests that also import source (cannot run against the Rust binary until these imports are removed):"
        );
        for (rel, reach) in &mixed {
            println!("  {rel}");
            for source in &reach.sources {
                println!("    imports {source}");
            }
        }
    }
    println!();
    if black_box.is_empty() {
        println!(
            "No test covers the program through its process boundary alone. Before porting, write tests that run the command (taken from an environment variable, so the same file runs against either build) and assert on stdout, stderr, the exit code, and the files written. `rustify compare` checks the same boundary for cases those tests leave out."
        );
    } else {
        println!(
            "Check by hand that these tests cover every command and flag, take the command to run from an environment variable, and need nothing a CI machine lacks (network, accounts, installed tools)."
        );
    }
    if !unmarked.is_empty() {
        println!(
            "\n{} of them match no `binary_test_markers` entry in rustify.toml, so `next` may hand them to a batch as module tests:",
            unmarked.len()
        );
        for rel in unmarked {
            println!("  {rel}");
        }
    }
    Ok(true)
}

/// Every test-marked file (cases and helpers) under the roots and the
/// workspace packages. The graph itself drops tests that import no source, which are
/// the ones this command looks for.
fn test_analyses(
    ws: &Workspace,
    graph: &Graph,
    analyzer: &Analyzer,
) -> BTreeMap<String, FileAnalysis> {
    let mut dirs: BTreeSet<String> = ws.config.roots.iter().cloned().collect();
    // Every workspace package: end-to-end tests often live in one of their
    // own, which no source file imports.
    dirs.extend(graph.packages.iter().map(|package| package.dir.clone()));
    let mut analyses = BTreeMap::new();
    for dir in &dirs {
        for rel in ts_files(ws, dir) {
            if !ws.is_test(&rel) || analyses.contains_key(&rel) {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(ws.root.join(&rel)) else {
                continue;
            };
            if let Ok(analysis) = analyzer.analyze(&rel, &source) {
                analyses.insert(rel, analysis);
            }
        }
    }
    analyses
}

/// What `test` reaches through its own imports and its helpers'.
fn reach(
    ws: &Workspace,
    graph: &Graph,
    analyses: &BTreeMap<String, FileAnalysis>,
    test: &str,
) -> Reach {
    let mut reach = Reach::default();
    let mut stack = vec![test.to_owned()];
    let mut visited = BTreeSet::new();
    while let Some(current) = stack.pop() {
        if !visited.insert(current.clone()) {
            continue;
        }
        let Some(analysis) = analyses.get(&current) else {
            continue;
        };
        let dir = current.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let owner = graph.packages.owner(&current);
        let text = std::fs::read_to_string(ws.root.join(&current)).unwrap_or_default();
        if GLOBAL_SPAWNS.iter().any(|call| text.contains(call)) {
            reach.spawns = true;
        }
        for import in &analysis.imports {
            let specifier = import.specifier.as_str();
            // Types are erased: the test still runs without the module.
            if import.kind == ImportKind::Type {
                continue;
            }
            // tsconfig path aliases point into the source tree.
            if ["@/", "~/", "#"]
                .iter()
                .any(|alias| specifier.starts_with(alias))
            {
                reach.sources.insert(specifier.to_owned());
                continue;
            }
            if specifier.starts_with('.') {
                // `../kit/dist/x.js` is another package's build output; look
                // for the source it was compiled from.
                let mut candidates = relative_candidates(dir, specifier);
                if specifier.contains("/dist/") {
                    candidates.extend(relative_candidates(
                        dir,
                        &specifier.replacen("/dist/", "/src/", 1),
                    ));
                }
                if let Some(helper) = candidates.iter().find(|c| analyses.contains_key(*c)) {
                    stack.push(helper.clone());
                } else if let Some(source) = candidates
                    .iter()
                    .find(|c| ws.is_source(c) && !ws.is_test(c) && ws.root.join(c).is_file())
                {
                    reach.sources.insert(source.clone());
                }
                continue;
            }
            let (name, _) = split_name(specifier);
            if let Some(package) = graph.packages.local(owner, name) {
                reach.sources.insert(package.name.clone());
                continue;
            }
            match graph.packages.link(owner, specifier) {
                Link::Workspace(package, _) => {
                    reach.sources.insert(package.name.clone());
                }
                Link::Npm(name) => {
                    let bare = specifier.strip_prefix("node:").unwrap_or(&name);
                    let bun_shell = specifier == "bun"
                        && import
                            .names
                            .iter()
                            .any(|n| matches!(n.as_str(), "$" | "spawn" | "spawnSync"));
                    if SPAWNERS.contains(&bare) || bun_shell {
                        reach.spawns = true;
                    } else if !specifier.starts_with("node:")
                        && specifier != "bun"
                        && !NODE_BUILTINS.contains(&specifier)
                        && !NODE_BUILTINS.contains(&name.as_str())
                        && !RUNNERS.contains(&name.as_str())
                        && !RUNNERS.contains(&specifier)
                    {
                        reach.needs.insert(name);
                    }
                }
            }
        }
    }
    reach
}
