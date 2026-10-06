//! Porting brief: everything an agent (or reviewer) needs to port one batch
//! without rediscovering it: where each file lands, what its imports already
//! map to, which TS constructs it uses and the Rust abstraction for each,
//! and which tests come along.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::components::{Components, WidgetStatus};
use crate::config::Workspace;
use crate::graph::{Graph, rust_target, snake};
use crate::index::Index;
use crate::mappings::Mappings;
use crate::plan::Batch;

pub fn render(
    ws: &Workspace,
    graph: &Graph,
    index: &Index,
    mappings: &Mappings,
    batch: &Batch,
) -> String {
    let rules: BTreeMap<&str, _> = mappings
        .constructs
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();
    let batch_files: BTreeSet<usize> = batch.files.iter().copied().collect();
    let mut out = String::new();

    let _ = writeln!(out, "# Port brief\n");
    let _ = writeln!(
        out,
        "{} file(s), {} TS lines. Unblocks {} unit(s) directly, {} downstream.\n",
        batch.files.len(),
        batch.lines,
        batch.unlocks,
        batch.downstream
    );
    let _ = writeln!(out, "| TypeScript | Rust file (crate src/) | Module |");
    let _ = writeln!(out, "|---|---|---|");
    for &f in &batch.files {
        let (file, module) = rust_target(ws, &graph.packages, &graph.files[f].rel);
        let _ = writeln!(out, "| `{}` | `{file}` | `{module}` |", graph.files[f].rel);
    }
    let cycles: Vec<&Vec<usize>> = batch
        .units
        .iter()
        .map(|&u| &graph.units[u].files)
        .filter(|files| files.len() > 1)
        .collect();
    for files in cycles {
        let names: Vec<&str> = files.iter().map(|&f| graph.files[f].rel.as_str()).collect();
        let _ = writeln!(
            out,
            "\nThese files import each other and must land together: {}",
            names.join(", ")
        );
    }

    for &f in &batch.files {
        let file = &graph.files[f];
        let (_, module) = rust_target(ws, &graph.packages, &file.rel);
        let _ = writeln!(out, "\n## `{}` → `{module}`\n", file.rel);

        if let Some(base) = index.upstream.stale.get(&file.rel) {
            write_update(&mut out, ws, index, &file.rel, base);
        } else if let Some(entry) = index.get(&file.rel).filter(|e| e.rust.is_some()) {
            // In the batch only because it shares an import cycle with a
            // stale module: its own TS did not change.
            let _ = writeln!(
                out,
                "**Already ported, unchanged upstream.** In this batch only because it imports, and is imported by, a module that changed; `{}` needs no edit unless that module's update forces one. Re-run `rustify done` on it with the rest of the batch.\n",
                entry.rust.as_deref().unwrap_or_default()
            );
            continue;
        } else if index.upstream.added.contains(&file.rel) {
            let _ = writeln!(
                out,
                "New upstream module: {} added it after the port began.\n",
                ws.upstream()
            );
        }

        if !file.analysis.exports.is_empty() {
            let _ = writeln!(out, "Exports:\n");
            for e in &file.analysis.exports {
                let rust_name = match e.kind {
                    "class" | "interface" | "type" | "enum" => e.name.clone(),
                    "const"
                        if e.name
                            .chars()
                            .all(|c| c.is_uppercase() || c == '_' || c.is_ascii_digit()) =>
                    {
                        e.name.clone()
                    }
                    _ => snake(&e.name),
                };
                let asyncness = if e.is_async { " (async)" } else { "" };
                let _ = writeln!(
                    out,
                    "- `{}` {}{asyncness} → `{module}::{rust_name}`",
                    e.name, e.kind
                );
            }
        }

        let mut internal = Vec::new();
        let mut types_first = Vec::new();
        for &d in &file.deps {
            if file.type_deps.contains(&d)
                && !batch_files.contains(&d)
                && !index
                    .status(&graph.files[d].rel)
                    .is_some_and(|s| s.is_done())
            {
                let (rust_file, _) = rust_target(ws, &graph.packages, &graph.files[d].rel);
                let names: Vec<String> = file
                    .imported_names
                    .get(&d)
                    .map(|n| n.iter().map(|x| format!("`{x}`")).collect())
                    .unwrap_or_default();
                types_first.push(format!(
                    "- {} from `{}` → declare in `{rust_file}`",
                    if names.is_empty() {
                        "types".to_string()
                    } else {
                        names.join(", ")
                    },
                    graph.files[d].rel
                ));
                continue;
            }
            let dep = &graph.files[d];
            let line = if batch_files.contains(&d) {
                let (_, m) = rust_target(ws, &graph.packages, &dep.rel);
                format!("- `{}` → `{m}` (same batch)", dep.rel)
            } else if let Some(entry) = index.get(&dep.rel) {
                let target = entry
                    .module
                    .clone()
                    .or_else(|| entry.notes.clone())
                    .unwrap_or_default();
                let mut line = format!("- `{}` → `{target}` ({:?})", dep.rel, entry.status);
                let used: BTreeSet<&str> = file
                    .analysis
                    .imports
                    .iter()
                    .flat_map(|i| i.names.iter().map(String::as_str))
                    .collect();
                let symbols: Vec<String> = entry
                    .symbols
                    .iter()
                    .filter(|(ts, _)| used.contains(ts.as_str()))
                    .map(|(ts, rs)| format!("`{ts}` = `{rs}`"))
                    .collect();
                if !symbols.is_empty() {
                    let _ = write!(line, ": {}", symbols.join(", "));
                }
                line
            } else {
                format!("- `{}` → NOT PORTED (run `rustify next`)", dep.rel)
            };
            internal.push(line);
        }
        if !internal.is_empty() {
            let _ = writeln!(
                out,
                "\nImports from the graph (ported ones show their Rust paths):\n"
            );
            for line in internal {
                let _ = writeln!(out, "{line}");
            }
        }
        if !types_first.is_empty() {
            let _ = writeln!(
                out,
                "\nTypes from modules not ported yet (type-only imports). Declare just these types in the dependency's Rust file now, matching the TS shape; the rest of that module is ported later:\n"
            );
            for line in types_first {
                let _ = writeln!(out, "{line}");
            }
        }

        if !file.externals.is_empty() {
            let _ = writeln!(out, "\nExternal imports:\n");
            for name in &file.externals {
                match mappings.package_for(name) {
                    Some(rule) => {
                        let crates = if rule.crates.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", rule.crates.join(", "))
                        };
                        let _ = writeln!(out, "- `{name}` → {}{crates}", rule.rust);
                        if let Some(notes) = &rule.notes {
                            let _ = writeln!(out, "  - {notes}");
                        }
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "- `{name}` → NO MAPPING. Decide the crate, then add a [[package]] entry to mappings.toml."
                        );
                    }
                }
            }
        }
        if !file.unresolved.is_empty() {
            let list: Vec<&str> = file.unresolved.iter().map(String::as_str).collect();
            let _ = writeln!(
                out,
                "\nUnresolved imports (check by hand): {}",
                list.join(", ")
            );
        }

        let mut by_rule: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for hit in &file.analysis.hits {
            by_rule.entry(hit.rule.as_str()).or_default().push(hit.line);
        }
        if !by_rule.is_empty() {
            let _ = writeln!(out, "\nTypeScript constructs → Rust:\n");
            for (id, lines) in &by_rule {
                let Some(rule) = rules.get(id) else { continue };
                let shown: Vec<String> = lines.iter().take(8).map(|l| l.to_string()).collect();
                let more = if lines.len() > 8 {
                    format!(" (+{} more)", lines.len() - 8)
                } else {
                    String::new()
                };
                let _ = writeln!(
                    out,
                    "- **{}** (lines {}{more}): {}",
                    rule.title,
                    shown.join(", "),
                    rule.rust
                );
                for hazard in &rule.hazards {
                    let _ = writeln!(out, "  - Hazard: {hazard}");
                }
            }
        }
        if let Some(ui) = &ws.ui {
            ui_section(&mut out, ui, graph, f);
        }
    }

    let _ = writeln!(out, "\n## Tests to port with this batch\n");
    if batch.tests.is_empty() {
        let _ = writeln!(
            out,
            "None become portable yet. Write Rust unit tests for the exported behavior; the TS tests for these modules also need modules that are not ported."
        );
    } else {
        for &t in &batch.tests {
            let test = &graph.tests[t];
            let _ = writeln!(out, "- `{}` ({} lines)", test.rel, test.analysis.lines);
            for helper in &test.helpers {
                let _ = writeln!(out, "  - helper: `{helper}`");
            }
        }
        let _ = writeln!(
            out,
            "\nPort each case one-for-one (same name, same assertions). Record them with `rustify done ... --test <file>`."
        );
    }

    let names: Vec<&str> = batch
        .files
        .iter()
        .map(|&f| graph.files[f].rel.as_str())
        .collect();
    let _ = writeln!(out, "\n## Workflow\n");
    let _ = writeln!(out, "1. `rustify start {}`", names.join(" "));
    let _ = writeln!(
        out,
        "2. Port into the Rust files above, add the `mod` declarations, and port the tests."
    );
    let mut packages: Vec<String> = batch
        .files
        .iter()
        .map(|&f| {
            let (_, module) = rust_target(ws, &graph.packages, &graph.files[f].rel);
            cargo_package(ws, &module)
        })
        .collect();
    packages.sort();
    packages.dedup();
    let _ = writeln!(
        out,
        "3. {} and `cargo clippy --workspace --all-targets -- -D warnings`.",
        packages
            .iter()
            .map(|p| format!("`cargo test -p {p}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = writeln!(
        out,
        "4. `rustify done {}` (records the symbol map), then `rustify check`.",
        names.join(" ")
    );
    if let Some(doc) = &ws.config.conventions {
        let _ = writeln!(out, "\nConventions: {doc}.");
    }
    out
}

/// Cargo package that owns `module`: the library crate's directory name, or
/// the primary crate.
fn cargo_package(ws: &Workspace, module: &str) -> String {
    let first = module.split("::").next().unwrap_or(module);
    match &ws.ui {
        Some(ui) if ui.library.crate_name == first => ui
            .library
            .krate
            .rsplit('/')
            .next()
            .unwrap_or(&ui.library.krate)
            .to_string(),
        _ => ws.config.rust_crate_name.clone(),
    }
}

/// Guidance for files that render terminal UI (`.tsx`, or importing Ink or
/// React): which library widgets to reuse, what each Ink/React API becomes,
/// and how to prove the port renders what Ink rendered.
fn ui_section(out: &mut String, ui: &Components, graph: &Graph, f: usize) {
    let file = &graph.files[f];
    let ink_names: BTreeSet<&str> = file
        .analysis
        .imports
        .iter()
        .filter(|i| ui.library.ui_packages.contains(&i.specifier))
        .flat_map(|i| i.names.iter().map(String::as_str))
        .collect();
    let is_ui = file.rel.ends_with(".tsx")
        || file.rel.ends_with(".jsx")
        || !ink_names.is_empty()
        || ui.module_for(&file.rel).is_some();
    if !is_ui {
        return;
    }
    let lib = &ui.library;
    let _ = writeln!(out, "\nTerminal UI (`{}`):\n", lib.crate_name);
    if ui.module_for(&file.rel).is_some() {
        let _ = writeln!(
            out,
            "- This module belongs to the component library. Keep it granular: one widget or primitive per module, palette passed in as a field (no global theme), rendering through ratatui's `Widget` trait, input through a `handle(&mut self, event) -> Action` method."
        );
    }
    let mut widgets = Vec::new();
    for rel in std::iter::once(&file.rel).chain(file.deps.iter().map(|&d| &graph.files[d].rel)) {
        for w in ui.widgets_from(rel) {
            widgets.push((w, rel));
        }
    }
    for (w, rel) in widgets {
        let state = match w.status {
            WidgetStatus::Available => "available, use it",
            WidgetStatus::Planned => "planned: build it in the library first, in its own PR",
        };
        let _ = writeln!(
            out,
            "- `{}` ({state}) replaces `{rel}`: {}",
            w.rust, w.summary
        );
    }
    let dependents = graph
        .files
        .iter()
        .filter(|g| g.deps.contains(&f) && (g.rel.ends_with(".tsx") || g.rel.ends_with(".jsx")))
        .count();
    if ui.module_for(&file.rel).is_none() && dependents >= 2 {
        let _ = writeln!(
            out,
            "- {dependents} other components import this one. Port it into the library instead: add a [[module]] for it to components.toml (and a [[widget]] entry) before `start`."
        );
    }
    let mut unmapped = Vec::new();
    for name in &ink_names {
        match ui.ink_rule(name) {
            Some(rule) => {
                let _ = writeln!(out, "- `{name}` → {}", rule.rust);
                for hazard in &rule.hazards {
                    let _ = writeln!(out, "  - Hazard: {hazard}");
                }
            }
            None => unmapped.push(*name),
        }
    }
    if !unmapped.is_empty() {
        let _ = writeln!(
            out,
            "- No [[ink]] rule for {}; decide the equivalent and add one to components.toml.",
            unmapped
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let package_dir = lib
        .golden_test
        .split("/src/")
        .next()
        .unwrap_or(&lib.golden_test);
    let test = lib
        .golden_test
        .strip_prefix(&format!("{package_dir}/"))
        .unwrap_or(&lib.golden_test);
    let command = lib.golden_command.replace("{test}", test);
    let _ = writeln!(
        out,
        "- Parity: add fixtures rendering this component to `{}`, then run `{command}` in `{package_dir}` to write `{}/<name>.txt`. The Rust test renders the same fixtures and compares with `{}::golden::assert_file`; record any intentional difference with its reason and the exact Rust frame. `rustify check` fails a ported `.tsx` whose Rust has no golden test.",
        lib.golden_test, lib.goldens, lib.crate_name
    );
}

/// The update half of a brief for a module ported from an older upstream:
/// the Rust exists, so the work is the diff since its `ts_base`.
fn write_update(out: &mut String, ws: &Workspace, index: &Index, rel: &str, base: &str) {
    use crate::provenance::diff;
    let upstream = ws.upstream();
    let entry = index.get(rel);
    let rust = entry
        .and_then(|e| e.rust.as_deref())
        .unwrap_or("its recorded Rust file");
    let mut paths = vec![rel];
    if let Some(e) = entry {
        paths.extend(e.tracked_tests());
    }
    let short = &base[..base.len().min(12)];
    let _ = writeln!(
        out,
        "**Update, not a fresh port.** Already ported to `{rust}` from `{short}`; {upstream} changed it since. Port the diff below into the existing Rust and its tests (keep the recorded tests' scenarios and add upstream's new ones), then re-run `rustify done {rel}` to re-stamp it.\n"
    );
    match diff(&ws.root, base, upstream, &paths) {
        Ok(patch) if !patch.is_empty() => {
            let _ = writeln!(out, "```diff\n{patch}\n```\n");
        }
        Ok(_) => {}
        Err(error) => {
            let _ = writeln!(
                out,
                "(diff unavailable: {error:#}; run `git diff {base} {upstream} -- {}`)\n",
                paths.join(" ")
            );
        }
    }
}
